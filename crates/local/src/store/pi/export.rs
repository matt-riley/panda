use super::*;

#[derive(Default)]
struct Link {
    parent: i64,
    depth: usize,
    entry: Option<String>,
    tail: Option<String>,
    calls: Vec<panda_core::Call>,
    thinking: Option<String>,
}

fn tool_name<'a>(nodes: &'a HashMap<i64, Link>, mut parent: i64, id: &str) -> Option<&'a str> {
    while parent != 0 {
        let node = &nodes[&parent];
        if let Some(call) = node.calls.iter().find(|c| c.id == id) {
            return Some(&call.name);
        }
        parent = node.parent;
    }
    None
}

pub(super) fn write_entries(
    conn: &Connection,
    sid: i64,
    tip: i64,
    now: &str,
    ms: i64,
    name: &str,
    out: &mut impl Write,
) -> Result<()> {
    let mut ids=conn.prepare("SELECT external_id FROM pi_records JOIN tree_nodes ON id=node WHERE session=? AND external_id IS NOT NULL")?;
    let used = ids
        .query_map([sid], |r| r.get(0))?
        .collect::<rusqlite::Result<HashSet<String>>>()?;
    let fresh_id = |key: &str| {
        let mut id = format!("panda-{key}");
        while used.contains(&id) {
            id.push('-');
        }
        id
    };
    // Panda stores reasoning separately. Where it belongs to one following
    // answer, serialize both as content blocks of that Pi assistant message.
    let mut fold = conn.prepare(
        "SELECT parent FROM tree_nodes WHERE session=? GROUP BY parent
        HAVING count(*)=1 AND json_type(message,'$.Assistant') IS NOT NULL",
    )?;
    let single_answer = fold
        .query_map([sid], |r| r.get(0))?
        .collect::<rusqlite::Result<HashSet<i64>>>()?;
    let mut nodes = HashMap::from([(0, Link::default())]);
    // Panda has no historical per-message timestamps. Assign one on first
    // export so exporting a changed branch never re-dates its siblings.
    conn.execute(
        "INSERT OR IGNORE INTO pi_export_times
        SELECT n.id,?2,?3 FROM tree_nodes n LEFT JOIN pi_records r ON n.id=r.node
        WHERE n.session=?1 AND r.node IS NULL",
        params![sid, now, ms],
    )?;
    let mut stmt = conn.prepare(
        "SELECT n.id,n.parent,n.depth,n.message,n.checkpoint,n.usage,r.node,r.external_id,r.raw,t.stamp,t.millis
        FROM tree_nodes n LEFT JOIN pi_records r ON r.node=n.id LEFT JOIN pi_export_times t ON t.node=n.id
        WHERE session=? ORDER BY n.id",
    )?;
    let mut rows = stmt.query([sid])?;
    let mut last = None;
    while let Some(r) = rows.next()? {
        let node = r.get::<_, i64>(0)?;
        let parent = r.get::<_, i64>(1)?;
        let depth = r.get::<_, usize>(2)?;
        let msg: Option<Message> = serde_json::from_str(&r.get::<_, String>(3)?)?;
        let cp = r
            .get::<_, Option<String>>(4)?
            .map(|s| serde_json::from_str::<Checkpoint>(&s))
            .transpose()?;
        let usage = r
            .get::<_, Option<String>>(5)?
            .map(|s| serde_json::from_str::<UsageRecord>(&s))
            .transpose()?;
        let imported = r.get::<_, Option<i64>>(6)?.is_some();
        let mut entry = r.get::<_, Option<String>>(7)?;
        let raw = r.get::<_, Option<String>>(8)?;
        let stamp = r.get::<_, Option<String>>(9)?;
        let node_time = stamp.as_deref().unwrap_or(now);
        let millis = r.get::<_, Option<i64>>(10)?.unwrap_or(ms);
        let mut tail = nodes[&parent].tail.clone();
        let mut thinking = None;
        let mut original_cp = false;
        if let Some(raw) = raw {
            writeln!(out, "{raw}")?;
            tail = entry.clone();
            original_cp = serde_json::from_str::<Value>(&raw)?["type"] == "compaction";
        } else if !imported
            && let Some(Message::Reasoning(text)) = &msg
            && cp.is_none()
            && tip != node
            && single_answer.contains(&node)
        {
            thinking = Some(text.clone());
        } else if !imported && let Some(m) = &msg {
            let id = fresh_id(&node.to_string());
            let mut record = match m {
                Message::BranchSummary(summary) => {
                    json!({"type":"branch_summary","summary":summary,"fromId":tail})
                }
                _ => {
                    let name = if let Message::Result { id, .. } = m {
                        tool_name(&nodes, parent, id)
                    } else {
                        None
                    };
                    messages::encode(
                        m,
                        millis,
                        name,
                        usage.as_ref(),
                        nodes[&parent].thinking.as_deref(),
                    )?
                }
            };
            record["id"] = json!(id);
            record["parentId"] = json!(tail);
            record["timestamp"] = json!(node_time);
            writeln!(out, "{record}")?;
            entry = Some(id);
            tail = entry.clone();
        }
        let calls = match msg {
            Some(Message::Assistant { calls, .. }) => calls,
            _ => vec![],
        };
        nodes.insert(
            node,
            Link {
                parent,
                depth,
                entry,
                tail: tail.clone(),
                calls,
                thinking,
            },
        );
        if let Some(cp) = cp
            && !original_cp
        {
            let id = fresh_id(&format!("cp-{node}"));
            let mut kept = id.clone();
            let mut p = node;
            while nodes[&p].depth > cp.through {
                if let Some(e) = &nodes[&p].entry {
                    kept = e.clone();
                }
                p = nodes[&p].parent;
            }
            writeln!(
                out,
                "{}",
                json!({"type":"compaction","id":id,"parentId":tail,"timestamp":node_time,
                "summary":cp.summary,"firstKeptEntryId":kept,"tokensBefore":0})
            )?;
            tail = Some(id);
            nodes.get_mut(&node).unwrap().tail = tail.clone();
        }
        last = tail;
    }
    let active = &nodes[&tip].tail;
    if name != imported_name(conn, sid)? {
        writeln!(
            out,
            "{}",
            json!({"type":"session_info","name":name,"id":fresh_id("name"),"parentId":active,"timestamp":now})
        )?;
    } else if active != &last {
        // Pi selects the final record on reopen, even for an interior branch.
        writeln!(
            out,
            "{}",
            json!({"type":"custom","customType":"panda.active-branch","data":{},
            "id":fresh_id("head"),"parentId":active,"timestamp":now})
        )?;
    }
    Ok(())
}
