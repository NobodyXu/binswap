//! Download and swap binaries from GitHub
//!
//! # Usage
//!
//! `binswap` uses the same infrastructure as
//! [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) to
//! determine where the latest binaries are stored. `binswap-github` is the
//! backend to do this for GitHub specifically. It uses the GitHub releases to
//! download binaries for a supported target, and then downloads them to a
//! specified location, or optionally swaps them with the currently executed
//! binary.
//!
//! This is particularly useful if you distribute binaries outside of package
//! managers or in environments where the users are not expected to have Rust
//! nor installed. With crate, you can bundle the updating mechanism into the
//! distributed binary.
//!
//! # Example
//!
//! The following example downloads the latest release [`ripgrep` from
//! GitHub](https://github.com/BurntSushi/ripgrep/releases), and swaps it with
//! the currently executed binary.
//!
//! ```
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     binswap_github::builder()
//!         .repo_author("BurntSushi")
//!         .repo_name("ripgrep")
//!         .asset_name("ripgrep")
//!         .bin_name("rg")
//!         .build()?
//!         .fetch_and_write_in_place_of_current_exec()
//!         .await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! The following does the same, but just writes the resulting binary to a new
//! file.
//!
//! ```
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     binswap_github::builder()
//!         .repo_author("BurntSushi")
//!         .repo_name("ripgrep")
//!         .asset_name("ripgrep")
//!         .bin_name("rg")
//!         .build()?
//!         .fetch_and_write_to("./rg")
//!         .await?;
//!
//!     Ok(())
//! }
//! ```

#![warn(missing_docs)]

use std::{
    io::{self, stderr, BufRead, StdinLock},
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use binstalk::{
    fetchers::{Data, Fetcher, GhCrateMeta, TargetData},
    get_desired_targets,
    helpers::remote::Client,
    manifests::cargo_toml_binstall::PkgMeta,
};
use color_eyre::{eyre::eyre, Result};
use crossterm::{
    cursor::{RestorePosition, SavePosition},
    style::{Print, ResetColor, Stylize},
    ExecutableCommand,
};
use derive_builder::Builder;
use serde::Deserialize;
use tokio::sync::oneshot;

/// Create a new builder. Finish by calling `.build()`
pub fn builder() -> BinswapGithubBuilder {
    Default::default()
}

/// The parameters used to fetch and install binaries
#[derive(Debug, Clone, Builder)]
pub struct BinswapGithub {
    /// The name of the author or team of the repository on GitHub.
    #[builder(setter(into))]
    repo_author: String,
    /// The name of the repository on GitHub.
    #[builder(setter(into))]
    repo_name: String,
    /// The name of the asset in the release. If not given `bin_name` will be
    /// used.
    #[builder(setter(into, strip_option), default)]
    asset_name: Option<String>,
    /// The name of the binary in the release.
    #[builder(setter(into))]
    bin_name: String,
    /// The desired version to download. If not given the latest will be used.
    #[builder(setter(into, strip_option), default)]
    version: Option<String>,
    /// The command to run to check that the binary is executable before
    /// installing it.
    #[builder(setter(into), default = "\"--help\".to_string()")]
    check_with_cmd: String,
    /// Do not run the check command be fore installing.
    #[builder(setter(into), default = "false")]
    no_check_with_cmd: bool,
}

impl BinswapGithub {
    /// Downloads and writes the found binary to the location of the currently
    /// executed binary in-place.
    ///
    /// ### Warning
    ///
    /// This action alters the binary and is **not reversible**!
    pub async fn fetch_and_write_in_place_of_current_exec(&self) -> Result<()> {
        self.fetch_and_write_to(std::env::current_exe()?).await
    }
    /// Downloads and writes the found binary to the specified location.
    pub async fn fetch_and_write_to(&self, target_binary: impl AsRef<Path>) -> Result<()> {
        let target_binary = target_binary.as_ref();

        let name = target_binary
            .file_name()
            .ok_or_else(|| eyre!("target file had no name"))?
            .to_str()
            .unwrap();

        let temp = tempfile::Builder::new().prefix("binswap").tempdir()?;

        let client = Client::new(
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
            None,
            Duration::from_millis(5),
            NonZeroU64::new(1).unwrap(),
        )?;

        stderr()
            .execute(Print("Updating ".green()))?
            .execute(Print(&name))?
            .execute(Print("...\n".green()))?
            .execute(ResetColor)?;

        let version = if let Some(v) = self.version.clone() {
            v
        } else {
            #[derive(Debug, Deserialize)]
            struct Response {
                tag_name: String,
            }

            stderr()
                .execute(Print(
                    "Getting latest version number...\n".magenta().italic(),
                ))?
                .execute(ResetColor)?;

            let res: Response = client
                .get_inner()
                .get(format!(
                    "https://api.github.com/repos/{}/{}/releases/latest",
                    self.repo_author, self.repo_name
                ))
                .send()
                .await?
                .json()
                .await?;
            res.tag_name.trim_start_matches('v').to_string()
        };

        stderr()
            .execute(Print("Using version ".green()))?
            .execute(Print(&version))?
            .execute(Print("\n"))?
            .execute(ResetColor)?;

        for target in get_desired_targets(None).get().await {
            let resolver = GhCrateMeta::new(
                client.clone(),
                Arc::new(Data {
                    name: self
                        .asset_name
                        .clone()
                        .unwrap_or_else(|| self.bin_name.clone())
                        .into(),
                    version: version.clone().into(),
                    repo: Some(format!(
                        "https://github.com/{}/{}/",
                        self.repo_author, self.repo_name
                    )),
                }),
                Arc::new(TargetData {
                    target: target.into(),
                    meta: PkgMeta {
                        pkg_url: None,
                        pkg_fmt: None,
                        bin_dir: None,
                        pub_key: None,
                        overrides: Default::default(),
                    },
                }),
            );

            stderr()
                .execute(Print("Looking for binary for target ".magenta().italic()))?
                .execute(Print(&target))?
                .execute(Print("...\n".magenta().italic()))?;

            let found = Arc::clone(&resolver).find().await??;
            if !found {
                continue;
            }

            stderr().execute(Print("Found a binary! Downloading...\n".magenta().italic()))?;

            resolver.fetch_and_extract(temp.path()).await?;

            let mut dir = tokio::fs::read_dir(temp.path()).await?;
            let bin_name = PathBuf::from(self.bin_name.clone());
            #[cfg(windows)]
            let bin_name = bin_name.with_extension("exe");
            let bin_path = temp.path().join(&bin_name);
            let mut bin_path = if tokio::fs::metadata(&bin_path).await.is_ok() {
                Some(bin_path)
            } else {
                None
            };
            if bin_path.is_none() {
                'bin_search: while let Some(entry) = dir.next_entry().await? {
                    if entry.file_type().await?.is_dir() {
                        let b = entry.path().join(&bin_name);
                        if tokio::fs::metadata(&b).await.is_ok() {
                            bin_path = Some(b);
                            break 'bin_search;
                        }
                    }
                }
            }

            if let Some(bin_path) = bin_path {
                if !self.no_check_with_cmd {
                    let res = tokio::process::Command::new(&bin_path)
                        .arg(&self.check_with_cmd)
                        .output()
                        .await?;
                    if !res.status.success() {
                        return Err(eyre!(
                            "Could not execute `{}` on downloaded binary",
                            self.check_with_cmd
                        ));
                    }
                }

                stderr()
                    .execute(Print("\n  About to write binary to ".green()))?
                    .execute(Print(format!("`{}`\n", target_binary.display())))?;

                if confirm().await {
                    stderr()
                        .execute(Print("\n".green()))?
                        .execute(Print(&name))?
                        .execute(Print(" has been updated!\n".green()))?
                        .execute(ResetColor)?;

                    tokio::fs::rename(bin_path, target_binary).await?;
                } else {
                    return Ok(());
                }

                return Ok(());
            } else {
                stderr().execute(Print(
                    " > No binary found in asset, trying next target...\n"
                        .red()
                        .italic(),
                ))?;
            }
        }

        Err(eyre!("not found"))
    }
}

fn ask_for_confirm(stdin: &mut StdinLock, input: &mut String) -> io::Result<()> {
    stderr()
        .execute(Print("\n  Do you wish to continue? ".yellow()))?
        .execute(Print("yes/[no]\n"))?
        .execute(Print("  ? ".dim()))?
        .execute(SavePosition)?
        .execute(Print("\n"))?
        .execute(RestorePosition)?;

    stdin.read_line(input)?;

    Ok(())
}

async fn confirm() -> bool {
    let (tx, rx) = oneshot::channel();

    thread::spawn(move || {
        // This task should be the only one able to
        // access stdin
        let mut stdin = io::stdin().lock();
        let mut input = String::with_capacity(16);

        let res = loop {
            if ask_for_confirm(&mut stdin, &mut input).is_err() {
                break false;
            }

            match input.as_str().trim() {
                "yes" | "y" | "YES" | "Y" => break true,
                "no" | "n" | "NO" | "N" | "" => break false,
                _ => {
                    input.clear();
                    continue;
                }
            }
        };

        // The main thread might be terminated by signal and thus cancelled
        // the confirmation.
        tx.send(res).ok();
    });

    rx.await.unwrap()
}
