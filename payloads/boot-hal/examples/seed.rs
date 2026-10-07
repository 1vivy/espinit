//! Emit tools/provision's --seed JSON; never opens a device or input image.
use esu_platform::efivars::PROJECT_GUID;
use gobbl_boot_hal::{Merge, State};
use std::fmt::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err("usage: seed <catalogue-id> <rom-number> <_a|_b>; slot must come from the installation receipt".into());
    }
    let id = &args[0];
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("invalid catalogue id".into());
    }
    let number: u32 = args[1].parse()?;
    let current = match args[2].as_str() {
        "_a" => 0,
        "_b" => 1,
        _ => return Err("expected _a or _b".into()),
    };
    let state = State::initial(number, current).map_err(|_| "ROM number must be nonzero")?;
    let merge = Merge {
        status: 0,
        source: current,
    };
    println!("[");
    for (index, (prefix, data)) in [
        ("Slot", state.encode().as_slice()),
        ("MergeStatus", merge.encode().as_slice()),
    ]
    .iter()
    .enumerate()
    {
        let mut hex = String::with_capacity(data.len() * 2);
        for byte in *data {
            write!(hex, "{byte:02x}")?;
        }
        println!(
            "  {{\"name\":\"{prefix}-{id}\",\"guid\":\"{PROJECT_GUID}\",\"attributes\":7,\"data_hex\":\"{hex}\"}}{}",
            if index == 0 { "," } else { "" }
        );
    }
    println!("]");
    Ok(())
}
