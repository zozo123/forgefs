#![no_main]

use forge_api::{dispatch_request, Forge};
use forge_protocol::{read_frame, Request};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use std::sync::OnceLock;

fn forge() -> &'static Forge {
    static FORGE: OnceLock<Forge> = OnceLock::new();
    FORGE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "forgefs-protocol-fuzz-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Forge::init(&dir).expect("initialize throwaway fuzz forge")
    })
}

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    let Ok(frame) = read_frame(&mut cursor) else {
        return;
    };
    let Ok(mut request) = serde_json::from_slice::<Request>(&frame) else {
        return;
    };

    // Capability parsing has its own target. Use a valid throwaway
    // root cap here so mutations reach operation/body dispatch too.
    request.cap = forge()
        .root_cap()
        .expect("read throwaway root cap")
        .to_token();
    let _ = dispatch_request(forge(), request);
});
