// AideRun - Win32 控制台按需附着/分配、UTF-8 ANSI 输出及图形错误弹窗辅助模块

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_UNKNOWN};
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetConsoleMode, GetConsoleWindow, GetStdHandle,
    SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, SetConsoleTitleW, SetStdHandle,
    ATTACH_PARENT_PROCESS, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 控制台准备模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleModeKind {
    /// 静默模式（仅错误弹窗，不弹出黑框）
    Silent,
    /// 优先附着父控制台；若无父控制台则新建带标题控制台窗口
    EnsureInteractive,
    /// 仅附着父控制台或保留现有管道，绝不强行弹窗
    AttachIfPresent,
}

/// 根据执行模式准备标准输入输出句柄并开启 UTF-8 / ANSI 颜色支持
pub fn init_console(mode: ConsoleModeKind, title: &str) -> bool {
    if mode == ConsoleModeKind::Silent {
        return false;
    }

    unsafe {
        let mut allocated = false;
        let has_valid_stdout = is_handle_valid(GetStdHandle(STD_OUTPUT_HANDLE));

        if !has_valid_stdout {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && mode == ConsoleModeKind::EnsureInteractive {
                if AllocConsole() != 0 {
                    allocated = true;
                }
            }
        }

        // 仅当标准句柄尚未被管道/文件重定向时才重绑到 CONOUT$/CONIN$
        rebind_std_handles_if_needed();

        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
        enable_vt_mode(STD_OUTPUT_HANDLE);
        enable_vt_mode(STD_ERROR_HANDLE);

        if allocated || GetConsoleWindow() != std::ptr::null_mut() {
            let wide = to_wide_null(title);
            SetConsoleTitleW(wide.as_ptr());
        }
        allocated
    }
}

unsafe fn is_handle_valid(handle: HANDLE) -> bool {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    GetFileType(handle) != FILE_TYPE_UNKNOWN
}

unsafe fn rebind_std_handles_if_needed() {
    if !is_handle_valid(GetStdHandle(STD_OUTPUT_HANDLE)) {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open("CONOUT$") {
            let h = file.as_raw_handle() as HANDLE;
            std::mem::forget(file);
            SetStdHandle(STD_OUTPUT_HANDLE, h);
        }
    }
    if !is_handle_valid(GetStdHandle(STD_ERROR_HANDLE)) {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open("CONOUT$") {
            let h = file.as_raw_handle() as HANDLE;
            std::mem::forget(file);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
    if !is_handle_valid(GetStdHandle(STD_INPUT_HANDLE)) {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open("CONIN$") {
            let h = file.as_raw_handle() as HANDLE;
            std::mem::forget(file);
            SetStdHandle(STD_INPUT_HANDLE, h);
        }
    }
}

unsafe fn enable_vt_mode(std_handle_id: u32) {
    let handle = GetStdHandle(std_handle_id);
    if !is_handle_valid(handle) {
        return;
    }
    let mut mode: u32 = 0;
    if GetConsoleMode(handle, &mut mode) != 0 {
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

/// 将 UTF-8 字符串转换为以 NUL 结尾的 UTF-16 序列
pub fn to_wide_null(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 静默模式下向用户弹出错误提示框
pub fn show_error_dialog(title: &str, message: &str) {
    let w_title = to_wide_null(title);
    let w_msg = to_wide_null(message);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), w_msg.as_ptr(), w_title.as_ptr(), MB_OK | MB_ICONERROR);
    }
}

/// 静默模式下向用户弹出信息提示框
pub fn show_info_dialog(title: &str, message: &str) {
    let w_title = to_wide_null(title);
    let w_msg = to_wide_null(message);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            w_msg.as_ptr(),
            w_title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// 读取终端单行输入（自动去除前后空白）
pub fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// 询问 Y/N 确认（default_yes 指定空回车默认行为）
pub fn prompt_confirm(prompt: &str, default_yes: bool) -> bool {
    let input = prompt_line(prompt).unwrap_or_default().to_ascii_lowercase();
    if input.is_empty() {
        return default_yes;
    }
    matches!(input.as_str(), "y" | "yes")
}

/// 交互界面暂停等待回车
pub fn pause_enter() {
    let _ = prompt_line("\n按回车键继续...");
}

