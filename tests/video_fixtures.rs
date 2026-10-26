//! Generated codec fixtures, not published Android vectors or a native decoder pass.
use meshlive::crypto::sha256;

#[test]
fn generated_video_fixture_packet_bytes_and_timestamps_match_frozen_manifest() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/video-lossless.json")).unwrap();
    for (codec, fourcc, data) in [
        (
            "vp9",
            b"VP90",
            include_bytes!("fixtures/video-vp9.ivf").as_slice(),
        ),
        (
            "av1",
            b"AV01",
            include_bytes!("fixtures/video-av1.ivf").as_slice(),
        ),
    ] {
        let hex = |bytes: &[u8]| {
            sha256(bytes)
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            hex(data),
            manifest["codecs"][codec]["sha256"].as_str().unwrap()
        );
        assert_eq!(
            &data[..12],
            &[b"DKIF".as_slice(), &[0, 0, 32, 0], fourcc].concat()
        );
        assert_eq!(u16::from_le_bytes(data[12..14].try_into().unwrap()), 64);
        assert_eq!(u16::from_le_bytes(data[14..16].try_into().unwrap()), 64);
        assert_eq!(u32::from_le_bytes(data[16..20].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(data[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(data[24..28].try_into().unwrap()), 3);
        let mut position = 32;
        for index in 0..3 {
            let size =
                u32::from_le_bytes(data[position..position + 4].try_into().unwrap()) as usize;
            let ticks = u64::from_le_bytes(data[position + 4..position + 12].try_into().unwrap());
            position += 12;
            let packet = &data[position..position + size];
            let expected = &manifest["codecs"][codec]["packets"][index];
            assert_eq!(size as u64, expected["bytes"].as_u64().unwrap());
            assert_eq!(ticks, index as u64);
            assert_eq!(hex(packet), expected["sha256"].as_str().unwrap());
            assert_eq!(
                manifest["frames"][index]["pts_us"].as_u64().unwrap(),
                ticks * 200000
            );
            position += size;
        }
        assert_eq!(position, data.len());
    }
}
