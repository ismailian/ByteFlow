//! Standalone encrypt/decrypt CLI — the Rust twin of `bin/vault_crypto.dart`
//! (spec §9.11). Local file → local file only, never R2. Produces and reads
//! v2 only; output interoperates with the mobile app, the Dart CLI and the
//! desktop app.
//!
//! ```text
//! byteflow-crypto -e|-d [-p <passphrase>] [-o out] [-f] <file>
//! ```
//! Without `-p` the passphrase is read from stdin (keeps it out of the
//! process list and shell history). Never overwrites without `-f`. A wrong
//! passphrase and a tampered file fail identically.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bf_core::crypto::Argon2Params;
use bf_core::crypto::file_crypto::{decrypt_file_with_passphrase, encrypt_file_with_passphrase, is_encrypted_file};

const USAGE: &str = "usage: byteflow-crypto -e|-d [-p <passphrase>] [-o <out>] [-f] <file>";

struct Args {
    encrypt: bool,
    passphrase: Option<zeroize::Zeroizing<String>>,
    out: Option<PathBuf>,
    force: bool,
    input: PathBuf,
}

fn parse() -> Result<Args, String> {
    let mut mode = None;
    let (mut pass, mut out, mut force, mut input) = (None, None, false, None);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-e" | "--encrypt" => mode = Some(true),
            "-d" | "--decrypt" => mode = Some(false),
            "-p" | "--passphrase" => pass = Some(zeroize::Zeroizing::new(it.next().ok_or("-p needs a value")?)),
            "-o" | "--output" => out = Some(PathBuf::from(it.next().ok_or("-o needs a value")?)),
            "-f" | "--force" => force = true,
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            s => {
                if input.replace(PathBuf::from(s)).is_some() {
                    return Err(format!("only one input file\n{USAGE}"));
                }
            }
        }
    }
    Ok(Args {
        encrypt: mode.ok_or(USAGE)?,
        passphrase: pass,
        out,
        force,
        input: input.ok_or(USAGE)?,
    })
}

/// `<file>.enc` for encrypt; for decrypt strip `.enc`, otherwise `.dec`.
fn default_output(input: &Path, encrypt: bool) -> PathBuf {
    let s = input.as_os_str().to_string_lossy();
    if encrypt {
        PathBuf::from(format!("{s}.enc"))
    } else if let Some(stripped) = s.strip_suffix(".enc") {
        PathBuf::from(stripped)
    } else {
        PathBuf::from(format!("{s}.dec"))
    }
}

fn read_passphrase() -> Result<zeroize::Zeroizing<String>, String> {
    eprint!("Passphrase: ");
    let _ = std::io::stderr().flush();
    let mut line = zeroize::Zeroizing::new(String::new());
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    let trimmed = zeroize::Zeroizing::new(line.trim_end_matches(['\r', '\n']).to_string());
    if trimmed.is_empty() {
        return Err("empty passphrase".into());
    }
    Ok(trimmed)
}

fn run() -> Result<(), String> {
    let a = parse()?;
    if !a.input.is_file() {
        return Err(format!("no such file: {}", a.input.display()));
    }
    if !a.encrypt && !is_encrypted_file(&a.input) {
        return Err("not an encrypted file".into());
    }
    let out = a.out.unwrap_or_else(|| default_output(&a.input, a.encrypt));
    if out.exists() && !a.force {
        return Err(format!("{} exists (use -f to overwrite)", out.display()));
    }
    let pass = match a.passphrase {
        Some(p) => p,
        None => read_passphrase()?,
    };
    let r = if a.encrypt {
        encrypt_file_with_passphrase(&a.input, &out, pass.as_bytes(), Argon2Params::default(), &mut |_, _| true)
    } else {
        decrypt_file_with_passphrase(&a.input, &out, pass.as_bytes(), &mut |_, _| true)
    };
    match r {
        Ok(()) => {
            println!("{}", out.display());
            Ok(())
        }
        Err(e) => Err(match e {
            bf_core::AppFailure::Crypto { message } if message.contains("legacy format") => message,
            bf_core::AppFailure::Crypto { .. } => "decryption failed: wrong passphrase or damaged file".into(),
            other => other.developer_message().to_string(),
        }),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outputs() {
        assert_eq!(default_output(Path::new("a.txt"), true), PathBuf::from("a.txt.enc"));
        assert_eq!(default_output(Path::new("a.txt.enc"), false), PathBuf::from("a.txt"));
        assert_eq!(default_output(Path::new("a.bin"), false), PathBuf::from("a.bin.dec"));
    }
}
