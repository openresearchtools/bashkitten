use base64::{Engine as _, engine::general_purpose::STANDARD};
use bashkitten::image::{self, ResizeOptions};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[test]
fn pinned_pi_image_detection_conversion_resize_orientation_and_bytes() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-images.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = STANDARD.decode(case["bytes"].as_str().unwrap()).unwrap();
        assert_eq!(
            image::detect_mime(&bytes),
            case["detected"].as_str(),
            "{name}: detection"
        );
        let opts = &case["options"];
        let resize: ResizeOptions = opts
            .get("resizeOptions")
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .unwrap_or_default();
        let result = image::process(
            &bytes,
            case["mime"].as_str().unwrap(),
            opts["autoResizeImages"].as_bool().unwrap_or(true),
            resize,
        );
        let expected = &case["expected"];
        if !expected["ok"].as_bool().unwrap() {
            assert_eq!(
                result.unwrap_err(),
                expected["message"].as_str().unwrap(),
                "{name}"
            );
            continue;
        }
        let result = result.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(result.mime_type, expected["mimeType"], "{name}: MIME");
        assert_eq!(
            serde_json::to_value(result.hints).unwrap(),
            expected["hints"],
            "{name}: hints"
        );
        let bytes = STANDARD.decode(result.data).unwrap();
        assert_eq!(
            bytes.len() as u64,
            expected["byteLength"].as_u64().unwrap(),
            "{name}: encoded size"
        );
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            expected["sha256"],
            "{name}: encoded bytes"
        );
    }
}
