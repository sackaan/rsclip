//! Enumerate processes currently producing audio.
//!
//! Walks the WASAPI audio session list on the default render endpoint —
//! same source OBS uses for its "Application Audio Capture" picker. Only
//! processes that have an active session show up; idle apps are skipped.
//!
//! Multi-process apps (browsers, Discord, Slack, etc.) spawn many helper
//! PIDs that each open their own audio sessions. Showing them all is
//! confusing. We coalesce sessions sharing an executable name down to a
//! single picker entry whose PID is the **eldest ancestor** with the same
//! exe — i.e. the root browser/app process. Capturing that root with
//! `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` (already the
//! default in `wasapi::process_loopback_loop`) sweeps in every helper PID.

use std::collections::HashMap;

use anyhow::{Context, Result};
use log::debug;

use windows::core::Interface;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

/// One audio session backed by a real process.
#[derive(Clone, Debug)]
pub struct AudioSession {
    /// OS process id producing the audio. After coalescing this is the
    /// eldest ancestor sharing the same executable as the original session.
    pub pid: u32,
    /// Executable basename, lowercased (e.g. `"discord.exe"`). Used to
    /// re-resolve sessions across launches.
    pub exe_name: String,
    /// Display name shown in the picker. Falls back to `exe_name` when the
    /// session does not provide a friendlier name.
    pub display_name: String,
}

/// One entry in the toolhelp process snapshot.
struct ProcInfo {
    parent_pid: u32,
    exe_name: String,
}

/// Enumerate active audio sessions on the default render endpoint,
/// coalesced by executable name.
///
/// Filters out: our own process, and PIDs in `excluded_pids` (typically
/// RuneLite — already covered by the dedicated Game channel).
pub fn enumerate_audio_sessions(excluded_pids: &[u32]) -> Result<Vec<AudioSession>> {
    unsafe { enumerate_inner(excluded_pids) }
}

unsafe fn enumerate_inner(excluded_pids: &[u32]) -> Result<Vec<AudioSession>> {
    let process_map = snapshot_processes().unwrap_or_default();

    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eMultimedia)
        .context("GetDefaultAudioEndpoint(eRender) failed")?;

    let manager: IAudioSessionManager2 = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate(IAudioSessionManager2) failed")?;

    let enum_sessions = manager
        .GetSessionEnumerator()
        .context("GetSessionEnumerator failed")?;

    let count = enum_sessions.GetCount().context("GetCount failed")?;
    let own_pid = GetCurrentProcessId();

    // Coalesce by exe — first session wins for display_name, root_pid is
    // recomputed from the process tree.
    let mut by_exe: HashMap<String, AudioSession> = HashMap::new();

    for i in 0..count {
        let control = match enum_sessions.GetSession(i) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let control2: IAudioSessionControl2 = match control.cast() {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Note: we intentionally don't call `IsSystemSoundsSession()` here —
        // windows-rs maps both S_OK and S_FALSE to Ok(()), so the boolean
        // is lost. The pid==0 / missing-exe filters below cover it.

        let session_pid = match control2.GetProcessId() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if session_pid == 0 || session_pid == own_pid {
            continue;
        }

        let exe_name = match process_map.get(&session_pid) {
            Some(info) => info.exe_name.clone(),
            None => continue,
        };

        if exe_name.is_empty() {
            continue;
        }

        let root_pid = eldest_ancestor_with_exe(&process_map, session_pid, &exe_name);

        if excluded_pids.contains(&root_pid) || excluded_pids.contains(&session_pid) {
            continue;
        }

        // Prefer a display name that the session itself provided (only the
        // first session per exe contributes; later ones don't override).
        let display_name = session_display_name(&control2).unwrap_or_else(|| exe_name.clone());

        by_exe.entry(exe_name.clone()).or_insert(AudioSession {
            pid: root_pid,
            exe_name,
            display_name,
        });
    }

    let mut out: Vec<AudioSession> = by_exe.into_values().collect();
    out.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });

    debug!(
        "Enumerated {} unique audio-producing applications",
        out.len()
    );
    Ok(out)
}

/// Walk up the parent chain from `pid` while parents still share `exe_name`.
/// Returns the topmost matching ancestor (or `pid` itself if no parent matches).
fn eldest_ancestor_with_exe(map: &HashMap<u32, ProcInfo>, pid: u32, exe_name: &str) -> u32 {
    let mut current = pid;
    // Guard against pathological loops in malformed snapshots.
    for _ in 0..64 {
        let info = match map.get(&current) {
            Some(i) => i,
            None => return current,
        };
        let parent = info.parent_pid;
        if parent == 0 || parent == current {
            return current;
        }
        let parent_info = match map.get(&parent) {
            Some(i) => i,
            None => return current,
        };
        if parent_info.exe_name != exe_name {
            return current;
        }
        current = parent;
    }
    current
}

/// Read the session's display name. Sessions often leave this empty or set
/// it to a resource path like `@%SystemRoot%\System32\…,-202`; in those
/// cases we return `None` so the caller can fall back to the exe name.
unsafe fn session_display_name(control: &IAudioSessionControl2) -> Option<String> {
    let pwstr = control.GetDisplayName().ok()?;
    if pwstr.0.is_null() {
        return None;
    }
    let name = pwstr.to_string().ok();
    CoTaskMemFree(Some(pwstr.0 as *const _));
    let name = name?;
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.starts_with('@') {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Snapshot every running process and build a `pid → (parent_pid, exe)` map.
/// Uses `Toolhelp32Snapshot` so we can get the exe basename without needing
/// `PROCESS_QUERY_LIMITED_INFORMATION` on each PID — protected processes
/// like browsers' sandboxed renderers refuse `OpenProcess`.
unsafe fn snapshot_processes() -> Result<HashMap<u32, ProcInfo>> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
        .context("CreateToolhelp32Snapshot failed")?;

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    let mut map: HashMap<u32, ProcInfo> = HashMap::new();

    if Process32FirstW(snapshot, &mut entry).is_ok() {
        loop {
            let name_end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe_name = String::from_utf16_lossy(&entry.szExeFile[..name_end]).to_lowercase();

            map.insert(
                entry.th32ProcessID,
                ProcInfo {
                    parent_pid: entry.th32ParentProcessID,
                    exe_name,
                },
            );

            if Process32NextW(snapshot, &mut entry).is_err() {
                break;
            }
        }
    }

    let _ = CloseHandle(snapshot);
    Ok(map)
}
