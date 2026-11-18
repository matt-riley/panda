use panda_core::{Call, Event};
use panda_providers::Stream;
use proptest::prelude::*;
use serde_json::json;
proptest! {
 #[test]fn unicode_tool_arguments_round_trip(path in ".{0,200}", widths in prop::collection::vec(1usize..96,1..64)){
 let args=json!({"path":path});let mut bytes=Vec::new();
 for (i,c) in args.to_string().chars().enumerate(){let mut part=json!({"index":0,"function":{"arguments":c.to_string()}});
 if i==0{part["id"]=json!("a");part["function"]["name"]=json!("read")}
 bytes.extend(format!("data: {}\r\n\r\n",json!({"choices":[{"delta":{"tool_calls":[part]}}]})).bytes())}
 bytes.extend(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n");
 let mut stream=Stream::default();let mut position=0;
 for width in widths.iter().cycle(){let end=(position+width).min(bytes.len());stream.push(&bytes[position..end],&mut |_|{}).unwrap();position=end;if end==bytes.len(){break}}
 prop_assert_eq!(stream.finish().unwrap().1,vec![Call{id:"a".into(),name:"read".into(),args}]);
 }
 #[test]fn incomplete_stream_never_dispatches_payload(content in ".{0,150}"){
 let frame=format!("data: {}\n",json!({"choices":[{"delta":{"content":content},"finish_reason":"stop"}]}));
 let mut stream=Stream::default();let mut seen=String::new();stream.push(frame.as_bytes(),&mut |e|if let Event::Text(t)=e{seen.push_str(&t)}).unwrap();
 prop_assert_eq!(seen,content);prop_assert!(stream.finish().is_err());
 }
}
