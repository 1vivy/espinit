//! Minimal `/sys` readers: uevent attributes and directory listings.
//!
//! uevent contents are untrusted: only complete `KEY=VALUE` lines are kept.

use std::fs;
use std::io;
use std::path::Path;

/// Read and parse a sysfs `uevent` file.
pub fn uevent(path: &Path) -> io::Result<Vec<(String, String)>> {
    Ok(parse_uevent(&fs::read_to_string(path)?))
}

/// Split `KEY=VALUE` lines; malformed lines are dropped, never guessed.
pub fn parse_uevent(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// The `PARTNAME` value, when the attribute is present.
pub fn partname(entries: &[(String, String)]) -> Option<&str> {
    entries
        .iter()
        .find(|(key, _)| key == "PARTNAME")
        .map(|(_, value)| value.as_str())
}

/// Sorted names of the entries in a sysfs directory.
pub fn entries(path: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partname() {
        let entries = parse_uevent("MAJOR=259\nMINOR=3\nDEVNAME=espinit-gpt3\nPARTNAME=userdata\n");
        assert_eq!(partname(&entries), Some("userdata"));
    }

    #[test]
    fn missing_partname_is_none() {
        let entries = parse_uevent("MAJOR=8\nMINOR=15\nDEVNAME=sda15\nDEVTYPE=partition\n");
        assert_eq!(partname(&entries), None);
    }

    #[test]
    fn drops_malformed_lines() {
        let entries = parse_uevent("=value\nno-equals\nKEY=\n");
        assert_eq!(entries, vec![("KEY".to_string(), String::new())]);
    }
}
