//! INTEROP-11: the shared vector files that Android stores in its own repository
//! are vendored here byte for byte. Each is pinned to the Android source it
//! was copied from, so a changed copy (or a refresh that was not deliberate)
//! fails here. Refresh a file by copying Android's bytes, then update its pin
//! and the source date below; never edit a value to make a test pass.

const ANDROID_SOURCE: &str = "the Android source of 2026-11-10";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn the_vector_files_android_stores_are_the_android_bytes() {
    let files: [(&str, &[u8], &str); 4] = [
        (
            "app/src/test/resources/wifi-vectors.json",
            include_bytes!("vectors/conformance/wifi-vectors.json"),
            "3d1fc5217aedd5b70b5b44dafe5c0e745a4f14b767129d5fa8b0230fcffa39bb",
        ),
        (
            "app/src/test/resources/pc_announcement/pc_announcement_v1.json",
            include_bytes!("vectors/pc_announcement_v1.json"),
            "18b1ae844d4b2d2e255db56eff546cdf6e2844047a017ee2dc509b4cdec65f7c",
        ),
        (
            "app/src/test/resources/pc_announcement/pc_streams_v1.json",
            include_bytes!("vectors/pc_streams_v1.json"),
            "ff557bdda186cdc908ba1a3b85f73d65520666531e147f798fdced7ed0edb82c",
        ),
        (
            "tests/vectors/gr1_v1.json",
            include_bytes!("vectors/gr1_v1.json"),
            "210b2702415be2753a2e83cd710cda2397ce0636e773907bd32280b16fc25dcc",
        ),
    ];
    for (android_path, bytes, pinned) in files {
        let actual = hex(&meshlive::crypto::sha256(bytes).unwrap());
        assert_eq!(actual, pinned, "{android_path} is not the Android file of {ANDROID_SOURCE}");
    }
}
