pub fn tool_requires_approval(name: &str) -> bool {
    matches!(name, "run_shell" | "write_file" | "apply_patch")
}

pub fn approval_reason(name: &str) -> &'static str {
    match name {
        "run_shell" => "执行 shell 命令会修改 Docker 沙箱内工作区或产生副作用",
        "write_file" => "写文件会修改 Docker 沙箱内工作区内容",
        "apply_patch" => "apply_patch 会按补丁方式修改 Docker 沙箱内工作区文件",
        _ => "该工具默认需要人工确认",
    }
}
