// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::io::Write;
use std::process::{Command, ExitStatus, Stdio};
use std::str;

use thiserror::Error;

use crate::signing::{SigStatus, SignError, SigningBackend, Verification};

// Search for one of the:
//  [GNUPG:] GOODSIG <long keyid> <primary uid..>
//  [GNUPG:] EXPKEYSIG <long keyid> <primary uid..>
//  [GNUPG:] NO_PUBKEY <long keyid>
//  [GNUPG:] BADSIG <long keyid> <primary uid..>
// in the output from --status-fd=1
// Assume signature is invalid if none of the above was found
fn parse_gpg_verify_output(
    output: &[u8],
    allow_expired_keys: bool,
) -> Result<Verification, SignError> {
    output
        .split(|&b| b == b'\n')
        .filter_map(|line| line.strip_prefix(b"[GNUPG:] "))
        .find_map(|line| {
            let mut parts = line.splitn(3, |&b| b == b' ').fuse();
            let status = match parts.next()? {
                b"GOODSIG" => SigStatus::Good,
                b"EXPKEYSIG" => {
                    if allow_expired_keys {
                        SigStatus::Good
                    } else {
                        SigStatus::Bad
                    }
                }
                b"NO_PUBKEY" => SigStatus::Unknown,
                b"BADSIG" => SigStatus::Bad,
                _ => return None,
            };
            let key = parts
                .next()
                .and_then(|bs| str::from_utf8(bs).ok())
                .map(|value| value.trim().to_owned());
            let display = parts
                .next()
                .and_then(|bs| str::from_utf8(bs).ok())
                .map(|value| value.trim().to_owned());
            Some(Verification::new(status, key, display))
        })
        .ok_or(SignError::InvalidSignatureFormat)
}

#[derive(Debug)]
pub struct GpgBackend {
    program: OsString,
    allow_expired_keys: bool,
    extra_args: Vec<OsString>,
}

#[derive(Debug, Error)]
pub enum GpgError {
    #[error("GPG failed with exit status {exit_status}:\n{stderr}")]
    Command {
        exit_status: ExitStatus,
        stderr: String,
    },
    #[error("Failed to run GPG")]
    Io(#[from] std::io::Error),
}

impl From<GpgError> for SignError {
    fn from(e: GpgError) -> Self {
        SignError::Backend(Box::new(e))
    }
}

impl GpgBackend {
    pub fn new(program: OsString, allow_expired_keys: bool) -> Self {
        Self {
            program,
            allow_expired_keys,
            extra_args: vec![],
        }
    }

    /// Primarily intended for testing
    pub fn with_extra_args(mut self, args: &[OsString]) -> Self {
        self.extra_args.extend_from_slice(args);
        self
    }

    pub fn from_config(config: &config::Config) -> Self {
        Self::new(
            config
                .get_string("signing.backends.gpg.program")
                .unwrap_or_else(|_| "gpg".into())
                .into(),
            config
                .get_bool("signing.backends.gpg.allow-expired-keys")
                .unwrap_or(false),
        )
    }

    fn run(&self, input: &[u8], args: &[&OsStr], check: bool) -> Result<Vec<u8>, GpgError> {
        let process = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if check { Stdio::piped() } else { Stdio::null() })
            .args(&self.extra_args)
            .args(args)
            .spawn()?;
        process.stdin.as_ref().unwrap().write_all(input)?;
        let output = process.wait_with_output()?;
        if check && !output.status.success() {
            Err(GpgError::Command {
                exit_status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim_end().into(),
            })
        } else {
            Ok(output.stdout)
        }
    }
}

impl SigningBackend for GpgBackend {
    fn name(&self) -> &str {
        "gpg"
    }

    fn can_read(&self, signature: &[u8]) -> bool {
        signature.starts_with(b"-----BEGIN PGP SIGNATURE-----")
    }

    fn sign(&self, data: &[u8], key: Option<&str>) -> Result<Vec<u8>, SignError> {
        Ok(match key {
            Some(key) => self.run(data, &["-abu".as_ref(), key.as_ref()], true)?,
            None => self.run(data, &["-ab".as_ref()], true)?,
        })
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<Verification, SignError> {
        let mut signature_file = tempfile::Builder::new()
            .prefix(".jj-gpg-sig-tmp-")
            .tempfile()
            .map_err(GpgError::Io)?;
        signature_file.write_all(signature).map_err(GpgError::Io)?;
        signature_file.flush().map_err(GpgError::Io)?;

        let sig_path = signature_file.into_temp_path();

        let output = self.run(
            data,
            &[
                "--keyid-format=long".as_ref(),
                "--status-fd=1".as_ref(),
                "--verify".as_ref(),
                // the only reason we have those .as_refs transmuting to &OsStr everywhere
                sig_path.as_os_str(),
                "-".as_ref(),
            ],
            false,
        )?;

        parse_gpg_verify_output(&output, self.allow_expired_keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpg_verify_invalid_signature_format() {
        use assert_matches::assert_matches;
        assert_matches!(
            parse_gpg_verify_output(b"", true),
            Err(SignError::InvalidSignatureFormat)
        );
    }

    #[test]
    fn gpg_verify_bad_signature() {
        assert_eq!(
            parse_gpg_verify_output(b"[GNUPG:] BADSIG 123 456", true).unwrap(),
            Verification::new(SigStatus::Bad, Some("123".into()), Some("456".into()))
        );
    }

    #[test]
    fn gpg_verify_unknown_signature() {
        assert_eq!(
            parse_gpg_verify_output(b"[GNUPG:] NO_PUBKEY 123", true).unwrap(),
            Verification::new(SigStatus::Unknown, Some("123".into()), None)
        );
    }

    #[test]
    fn gpg_verify_good_signature() {
        assert_eq!(
            parse_gpg_verify_output(b"[GNUPG:] GOODSIG 123 456", true).unwrap(),
            Verification::new(SigStatus::Good, Some("123".into()), Some("456".into()))
        );
    }

    #[test]
    fn gpg_verify_expired_signature() {
        assert_eq!(
            parse_gpg_verify_output(b"[GNUPG:] EXPKEYSIG 123 456", true).unwrap(),
            Verification::new(SigStatus::Good, Some("123".into()), Some("456".into()))
        );

        assert_eq!(
            parse_gpg_verify_output(b"[GNUPG:] EXPKEYSIG 123 456", false).unwrap(),
            Verification::new(SigStatus::Bad, Some("123".into()), Some("456".into()))
        );
    }
}
