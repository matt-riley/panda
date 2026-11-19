use crate::attachments;
use anyhow::{Context, Result, bail, ensure};
use panda_core::*;
use panda_local::store::Disk;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read},
    path::Path,
    sync::{Mutex, mpsc},
    time::Duration,
};

#[derive(Default, PartialEq)]
pub enum Interface {
    #[default]
    Terminal,
    Print,
    Json,
    Rpc,
}
#[derive(Default)]
pub struct Options {
    pub interface: Interface,
    pub codemode: bool,
    pub resume: bool,
    pub prompt: String,
}
impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--codemode" => out.codemode = true,
                "--continue" | "-c" => out.resume = true,
                "--print" | "-p" => out.interface = Interface::Print,
                "--mode" => {
                    out.interface = match it.next().map(String::as_str) {
                        Some("json") => Interface::Json,
                        Some("rpc") => Interface::Rpc,
                        _ => bail!("expected --mode json or rpc"),
                    }
                }
                "--" => {
                    out.prompt = it.cloned().collect::<Vec<_>>().join(" ");
                    break;
                }
                x if x.starts_with('-') => bail!("unknown argument {x}"),
                _ => {
                    out.prompt = std::iter::once(arg)
                        .chain(it)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ");
                    break;
                }
            }
        }
        ensure!(
            out.interface != Interface::Rpc || out.prompt.is_empty(),
            "RPC reads requests on stdin"
        );
        if out.interface == Interface::Terminal && !out.prompt.is_empty() {
            out.interface = Interface::Print;
        }
        Ok(out)
    }
}

pub struct Host<'a> {
    pub db: &'a mut Disk,
    pub tools: &'a (dyn Toolset + Sync),
    pub connect: &'a dyn Fn() -> Result<Box<dyn Provider + Send>>,
    pub root: &'a Path,
    pub cancel: &'a Cancel,
}
impl Host<'_> {
    pub fn run(&mut self, options: Options) -> Result<()> {
        if options.interface == Interface::Rpc {
            return self.rpc();
        }
        let mut text = options.prompt;
        if text.is_empty() {
            io::stdin().read_to_string(&mut text)?;
        }
        ensure!(!text.trim().is_empty(), "missing prompt");
        let input = attachments::prepare(self.root, &text, None)?;
        let mut p = (self.connect)()?;
        let structured = options.interface == Interface::Json;
        turn_message(
            p.as_mut(),
            self.db,
            self.tools,
            input,
            self.cancel,
            &mut |e| {
                if structured {
                    println!("{}", json!({"type":"event","event":e}));
                }
            },
            &mut |_| vec![],
        )?;
        let s = self.db.load()?;
        let Some(Message::Assistant { text, .. }) = s.history.last() else {
            bail!("missing final response");
        };
        if structured {
            println!(
                "{}",
                json!({"type":"done","session_id":self.db.id(),"text":text})
            );
        } else {
            println!("{text}");
        }
        Ok(())
    }
    fn rpc(&mut self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        while let Ok(line) = rx.recv() {
            let Some(r) = decode(&line) else { continue };
            let id = &r["id"];
            let result = (|| -> Result<Value> {
                Ok(match r["type"].as_str() {
                    Some("session") => {
                        json!({"id":id,"type":"session","session_id":self.db.id(),"session":self.db.load()?})
                    }
                    Some("ack") => {
                        acknowledge(self.db)?;
                        json!({"id":id,"type":"ok"})
                    }
                    Some("prompt") => {
                        let text = r["text"].as_str().context("expected text")?;
                        let input = attachments::prepare(self.root, text, None)?;
                        let mut p = (self.connect)()?;
                        let queue = Mutex::new((Vec::new(), Vec::new()));
                        let queue_ref = &queue;
                        let cancel = self.cancel;
                        cancel.reset();
                        let db = &mut *self.db;
                        let tools = self.tools;
                        let run = std::thread::scope(|scope| -> Result<()> {
                            let worker = scope.spawn(move || {
                                turn_message(
                                    p.as_mut(),
                                    db,
                                    tools,
                                    input,
                                    cancel,
                                    &mut |e| {
                                        println!("{}", json!({"id":id,"type":"event","event":e}))
                                    },
                                    &mut |idle| {
                                        let mut q = queue_ref.lock().unwrap();
                                        let mut items = std::mem::take(&mut q.0);
                                        if idle && items.is_empty() {
                                            items.append(&mut q.1);
                                        }
                                        items
                                    },
                                )
                            });
                            while !worker.is_finished() {
                                match rx.recv_timeout(Duration::from_millis(20)) {
                                    Ok(line) => {
                                        if let Some(cmd) = decode(&line) {
                                            let cmd_id = &cmd["id"];
                                            let response = match cmd["type"].as_str() {
                                                Some("cancel") => {
                                                    cancel.stop();
                                                    json!({"id":cmd_id,"type":"ok"})
                                                }
                                                Some(kind @ ("steer" | "follow_up"))
                                                    if cmd["text"].is_string() =>
                                                {
                                                    let mut q = queue.lock().unwrap();
                                                    let dest = if kind == "steer" {
                                                        &mut q.0
                                                    } else {
                                                        &mut q.1
                                                    };
                                                    dest.push(cmd["text"].as_str().unwrap().into());
                                                    json!({"id":cmd_id,"type":"queued"})
                                                }
                                                _ => {
                                                    json!({"id":cmd_id,"type":"error","error":"turn active"})
                                                }
                                            };
                                            println!("{response}");
                                        }
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                }
                            }
                            worker
                                .join()
                                .map_err(|_| anyhow::anyhow!("turn worker failed"))?
                        });
                        let (steer, follow) = queue.into_inner().unwrap();
                        let undelivered = [steer, follow].concat();
                        match run {
                            Ok(()) => {
                                json!({"id":id,"type":"done","session_id":self.db.id(),"undelivered":undelivered})
                            }
                            Err(e) => {
                                json!({"id":id,"type":"error","error":e.to_string(),"undelivered":undelivered})
                            }
                        }
                    }
                    _ => bail!("expected session, prompt or ack"),
                })
            })();
            println!(
                "{}",
                result.unwrap_or_else(|e| json!({"id":id,"type":"error","error":e.to_string()}))
            );
        }
        Ok(())
    }
}
fn decode(line: &str) -> Option<Value> {
    match serde_json::from_str(line) {
        Ok(v) => Some(v),
        Err(e) => {
            println!("{}", json!({"type":"error","error":e.to_string()}));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panda_memory::{EmptySkills, Files, Reader};
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[test]
    fn fixture() {
        let Ok(mode) = std::env::var("PANDA_AUTOMATION_FIXTURE") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let coding = panda_coding::Coding {
            workspace: Files([("hello.txt".into(), "fixture answer".into())].into()),
            skills: EmptySkills,
        };
        Host {
            db: &mut Disk::open(root, root, None).unwrap(),
            tools: &coding,
            connect: &|| Ok(Box::new(Reader)),
            root,
            cancel: &Cancel::default(),
        }
        .run(
            Options::parse(&match mode.as_str() {
                "rpc" => vec!["--mode".into(), "rpc".into()],
                "json" => vec!["--mode".into(), "json".into(), "read".into()],
                _ => vec!["--print".into(), "read".into()],
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn scripted_tool_round_trip_in_each_automation_interface() {
        for mode in ["print", "json", "rpc"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "automation::tests::fixture", "--nocapture"])
                .env("PANDA_AUTOMATION_FIXTURE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            if mode == "rpc" {
                writeln!(
                    child.stdin.take().unwrap(),
                    "{}",
                    json!({"type":"prompt","id":7,"text":"read"})
                )
                .unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            let out = String::from_utf8(output.stdout).unwrap();
            if mode == "print" {
                assert!(out.lines().any(|l| l == "fixture answer"));
            } else {
                let events: Vec<Value> = out
                    .lines()
                    .filter(|l| l.starts_with('{'))
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
                assert!(events.iter().any(|v| v["event"]["type"] == "calling"));
                assert!(
                    events
                        .iter()
                        .any(|v| v["event"]["data"] == "fixture answer")
                );
                assert_eq!(events.last().unwrap()["type"], "done");
                if mode == "rpc" {
                    assert!(events.iter().all(|v| v["id"] == 7));
                }
            }
        }
    }
}
