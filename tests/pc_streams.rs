use meshlive::pc_announcement::{self, Pc};

#[test]
fn shared_pc_stream_vectors_match_android_and_preserve_inner_announcement_bytes() {
    let file: serde_json::Value = serde_json::from_str(include_str!("vectors/pc_streams_v1.json")).unwrap();
    let hex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
    for v in file["vectors"].as_array().unwrap() {
        let bytes = hex(v["hex"].as_str().unwrap());
        let result = pc_announcement::decode(&bytes);
        if let Some(error) = v["error"].as_str() {
            assert_eq!(format!("{:?}", result.unwrap_err()), error, "{}", v["name"]);
        } else {
            let (kind, inner) = match result.unwrap() {
                Pc::Call(c) => { assert_eq!(c.encode().unwrap().as_slice(), bytes); (0, None) },
                Pc::Request(a) => (1, Some(a)), Pc::Offer(a) => (2, Some(a)),
            };
            assert_eq!(kind, v["kind"].as_u64().unwrap() as u8);
            if let Some(a) = inner {
                assert_eq!(a.encode().unwrap().as_slice(), hex(v["inner"].as_str().unwrap()));
                assert_eq!(pc_announcement::wrap(kind, &a).unwrap().as_slice(), bytes);
            }
        }
    }
}
