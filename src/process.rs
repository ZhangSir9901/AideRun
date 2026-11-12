// AideRun - Win32 便携根目录进程占用探测模块（仅拦截当前便携目录自身锁定的进程）

use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 正在占用当前便携目录的进程信息
#[derive(Debug, Clone)]
pub struct ActivePortableProcess {
    pub pid: u32,
    pub name: String,
    pub exe_path: String,
}

/// 扫描正在运行且可执行文件路径位于 `portable_root` 内的进程（排除启动器自身）
pub fn find_active_processes_in_root(portable_root: &Path) -> Vec<ActivePortableProcess> {
    let mut active = Vec::new();
    let self_pid = std::process::id();
    let root_prefix = portable_root.display().to_string().to_ascii_lowercase();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return active;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let pid = entry.th32ProcessID;
                if pid != 0 && pid != self_pid {
                    if let Some(exe_path) = query_process_image(pid) {
                        let lower_path = exe_path.to_ascii_lowercase();
                        if lower_path.starts_with(&root_prefix) {
                            let name_len = entry
                                .szExeFile
                                .iter()
                                .position(|&c| c == 0)
                                .unwrap_or(entry.szExeFile.len());
                            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                            active.push(ActivePortableProcess {
                                pid,
                                name,
                                exe_path,
                            });
                        }
                    }
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
    }

    active
}

unsafe fn query_process_image(pid: u32) -> Option<String> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
    CloseHandle(handle);
    if ok == 0 || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

/// 仅过滤出 IDE 本体及语言服务器等关键占用进程
pub fn find_blocking_ide_processes(portable_root: &Path) -> Vec<ActivePortableProcess> {
    find_active_processes_in_root(portable_root)
        .into_iter()
        .filter(|p| {
            let n = p.name.to_ascii_lowercase();
            n.contains("antigravity")
                || n.contains("language_server")
                || n == "node.exe"
                || n == "python.exe"
                || n == "cargo.exe"
        })
        .collect()
}

