//!
//! # A Recover Mechanism for Blockchain Scene
//!
//! automatic operations:
//! - create a light-weight(COW) snapshot for each block
//! - clean up expired snapshots
//!

#![cfg(target_os = "linux")]
#![deny(warnings)]
#![deny(missing_docs)]

mod api;
mod driver;

pub use api::server::run_daemon;

use clap::Parser;
use driver::{btrfs, external, zfs};
use ruc::{cmd, *};
use std::{fmt, result::Result as StdResult, str::FromStr};

/// Maximum number of snapshots that can be kept
pub const CAP_MAX: u64 = 4096;

/// `itv.pow(i)`, only useful in `SnapAlgo::Fade` alfo
pub const STEP_CNT: usize = 10;

/// The co-responding VAR-name of `--snapshot-volume`
pub const ENV_VAR_BTM_VOLUME: &str = "BTM_VOLUME";

/// Config structure of snapshot
#[derive(Clone, Debug, Parser)]
pub struct BtmCfg {
    /// a global switch for enabling snapshot functions
    #[clap(long)]
    pub enable: bool,
    /// interval between adjacent snapshots, default to 10 blocks
    #[clap(short, long, default_value_t = 10)]
    pub itv: u64,
    /// the maximum number of snapshots that will be stored, default to 100
    #[clap(short, long, default_value_t = 100)]
    pub cap: u64,
    /// how many snapshots should be kept after a `clean_snapshots`
    pub cap_clean_kept: usize,
    /// Zfs or Btrfs or External, will try a guess if missing
    #[clap(short, long, default_value_t = SnapMode::Zfs)]
    pub mode: SnapMode,
    /// Fair or Fade, default to 'Fair'
    #[clap(short, long, default_value_t = SnapAlgo::Fair)]
    pub algo: SnapAlgo,
    /// a data volume containing both ledger data and tendermint data
    #[clap(short = 'p', long, default_value_t = String::from("zfs/data"))]
    pub volume: String,
}

impl Default for BtmCfg {
    fn default() -> Self {
        BtmCfg {
            enable: false,
            itv: 10,
            cap: 100,
            cap_clean_kept: 0,
            mode: SnapMode::Zfs,
            algo: SnapAlgo::Fair,
            volume: "zfs/data".to_owned(),
        }
    }
}

impl BtmCfg {
    /// create a simple instance
    #[inline(always)]
    pub fn new() -> Self {
        Self::new_enabled()
    }

    #[inline(always)]
    fn new_enabled() -> Self {
        BtmCfg {
            enable: true,
            ..Self::default()
        }
    }

    /// Used in client side
    #[inline(always)]
    pub fn new_client_hdr() -> Self {
        Self::new_enabled()
    }

    /// generate a snapshot for the latest state of blockchain
    #[inline(always)]
    pub fn snapshot(&self, idx: u64) -> Result<()> {
        alt!(!self.enable, return Ok(()));

        // sync data to disk before snapshoting
        nix::unistd::sync();

        match self.mode {
            SnapMode::Zfs => zfs::gen_snapshot(self, idx).c(d!()),
            SnapMode::Btrfs => btrfs::gen_snapshot(self, idx).c(d!()),
            SnapMode::External => external::gen_snapshot(self, idx).c(d!()),
        }
    }

    /// rollback the state of blockchain to a specificed height
    #[inline(always)]
    pub fn rollback(&self, idx: Option<u64>, strict: bool) -> Result<()> {
        match self.mode {
            SnapMode::Zfs => zfs::rollback(self, idx, strict).c(d!()),
            SnapMode::Btrfs => btrfs::rollback(self, idx, strict).c(d!()),
            SnapMode::External => Err(eg!("please use `btm` tool in `External` mode")),
        }
    }

    /// Get snapshot list in 'DESC' order.
    #[inline(always)]
    pub fn get_sorted_snapshots(&self) -> Result<Vec<u64>> {
        match self.mode {
            SnapMode::Zfs => zfs::sorted_snapshots(self).c(d!()),
            SnapMode::Btrfs => btrfs::sorted_snapshots(self).c(d!()),
            SnapMode::External => Err(eg!("please use `btm` tool in `External` mode")),
        }
    }

    /// try to guess a correct mode
    /// NOTE: not suitable for `External` mode
    #[inline(always)]
    pub fn guess_mode(volume: &str) -> Result<SnapMode> {
        zfs::check(volume)
            .c(d!())
            .map(|_| SnapMode::Zfs)
            .or_else(|e| btrfs::check(volume).c(d!(e)).map(|_| SnapMode::Btrfs))
    }

    #[inline(always)]
    fn get_cap(&self) -> u64 {
        alt!(self.cap > CAP_MAX, CAP_MAX, self.cap)
    }

    /// List all existing snapshots.
    pub fn list_snapshots(&self) -> Result<()> {
        println!("Available snapshots are listed below:");
        self.get_sorted_snapshots().c(d!()).map(|list| {
            list.into_iter().rev().for_each(|h| {
                println!("    {}", h);
            })
        })
    }

    /// Clean all existing snapshots.
    pub fn clean_snapshots(&self) -> Result<()> {
        self.get_sorted_snapshots().c(d!()).map(|list| {
            list.into_iter()
                .skip(self.cap_clean_kept)
                .rev()
                .for_each(|height| {
                    let cmd = match self.mode {
                        SnapMode::Btrfs => {
                            format!("btrfs subvolume delete {}@{}", &self.volume, height)
                        }
                        SnapMode::Zfs => format!("zfs destroy {}@{}", &self.volume, height),
                        _ => pnk!(Err(eg!("Unsupported deriver"))),
                    };
                    info_omit!(cmd::exec_output(&cmd));
                });
        })
    }
}

/// # Inner Operations
///
/// assume:
/// - root volume of zfs is `zfs`
/// - root volume of btrfs is `/btrfs`
/// - business data is stored in `<root volume>/data`
/// - target block height to recover is 123456
///
/// ## snapshot
///
/// ```shell
/// # zfs filesystem
/// zfs destroy zfs/data@123456 2>/dev/null
/// zfs snapshot zfs/data@123456
///
/// # btrfs filesystem
/// rm -rf /btrfs/data@123456 2>/dev/null
/// btrfs subvolume snapshot /btrfs/data /btrfs/data@123456
/// ```
///
/// ## rollback
///
/// ```shell
/// # zfs filesystem
/// zfs rollback -r zfs/data@123456
///
/// # btrfs filesystem
/// rm -rf /btrfs/data || exit 1
/// btrfs subvolume snapshot /btrfs/data@123456 /btrfs/data
/// ```
#[derive(Clone, Copy, Debug)]
pub enum SnapMode {
    /// available on some Linux distributions and FreeBSD
    /// - Ubuntu Linux
    /// - Gentoo Linux
    /// - FreeBSD
    /// - ...
    Zfs,
    /// available on most Linux distributions,
    /// but its user experience is worse than zfs
    Btrfs,
    /// TODO: unimplemented!
    /// rely on an external independent process
    External,
}

impl SnapMode {
    #[inline(always)]
    #[allow(missing_docs)]
    pub fn from_string(m: &str) -> Result<Self> {
        match m.to_lowercase().as_str() {
            "zfs" => Ok(Self::Zfs),
            "btrfs" => Ok(Self::Btrfs),
            "external" => Ok(Self::External),
            _ => Err(eg!()),
        }
    }
}

impl Default for SnapMode {
    fn default() -> Self {
        Self::Zfs
    }
}

impl fmt::Display for SnapMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let contents = match self {
            Self::Zfs => "Zfs",
            Self::Btrfs => "Btrfs",
            Self::External => "External",
        };
        write!(f, "{}", contents)
    }
}

impl FromStr for SnapMode {
    type Err = String;
    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        Self::from_string(s).c(d!()).map_err(|e| e.to_string())
    }
}

/// Snapshot management algorithm
#[derive(Clone, Copy, Debug)]
pub enum SnapAlgo {
    /// snapshots are saved at fixed intervals
    Fair,
    /// snapshots are saved in decreasing density
    Fade,
}

impl SnapAlgo {
    #[inline(always)]
    #[allow(missing_docs)]
    pub fn from_string(m: &str) -> Result<Self> {
        match m.to_lowercase().as_str() {
            "fair" => Ok(Self::Fair),
            "fade" => Ok(Self::Fade),
            _ => Err(eg!()),
        }
    }
}

impl Default for SnapAlgo {
    fn default() -> Self {
        Self::Fair
    }
}

impl fmt::Display for SnapAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let contents = match self {
            Self::Fair => "Fair",
            Self::Fade => "Fade",
        };
        write!(f, "{}", contents)
    }
}

impl FromStr for SnapAlgo {
    type Err = String;
    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        Self::from_string(s).c(d!()).map_err(|e| e.to_string())
    }
}
