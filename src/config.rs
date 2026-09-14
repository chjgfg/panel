// 配置文件的结构与查找。
use serde::Deserialize;

pub type BoxErr = Box<dyn std::error::Error>;

#[derive(Deserialize)]
pub struct Config {
    /// 直接监听公网口，浏览器访问 http://你的IP（80 端口不用写端口号）
    #[serde(default = "default_bind")]
    pub bind: String,
    /// 明文密码。配置文件记得 chmod 600
    pub password: String,
    /// 放项目的目录。里面每个子文件夹算一个项目，加项目不用改这里
    #[serde(default = "default_dirs")]
    pub dirs: Vec<String>,
    /// 不想在页面上看到的项目。三种写法：
    ///   "scratch"      项目名，任何扫描目录下叫这个的都排掉
    ///   "test-*"       通配符，只支持 *
    ///   "/srv/x/old"   带 / 就按完整路径匹配，只排掉这一个
    #[serde(default)]
    pub exclude: Vec<String>,
    /// cargo 的绝对路径。留空自动探测——systemd 起进程时 PATH 里
    /// 通常没有 ~/.cargo/bin，所以不能直接写 "cargo"
    #[serde(default)]
    pub cargo: Option<String>,
    /// 秘密路径前缀。设了之后只有 /前缀/... 能打开面板，其余路径一律 404，
    /// 公网上的扫描器/爆破工具看不出这台机器部署了面板。留空 = 挂在根路径
    #[serde(default)]
    pub prefix: String,
}
fn default_bind() -> String {
    "0.0.0.0:80".into()
}
fn default_dirs() -> Vec<String> {
    vec!["/opt/apps".into()]
}

/// 找配置文件，按这个顺序：
///   1. 环境变量 PANEL_CONFIG
///   2. 当前目录下的 panel.toml       —— 在源码目录里 cargo run / ./target/debug/panel
///   3. 可执行文件旁边的 panel.toml   —— 部署成 /opt/panel/{panel, panel.toml}
///   4. /etc/panel.toml
///
/// 2 和 3 缺一不可：cargo run 时可执行文件在 target/debug/ 里，跟你放配置的
/// 项目根目录不是一个地方；而 systemd 启动服务时工作目录是 /，第 2 条又指不到。
pub fn config_path() -> Result<std::path::PathBuf, BoxErr> {
    if let Ok(p) = std::env::var("PANEL_CONFIG") {
        return Ok(p.into());
    }
    let mut tried = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        tried.push(cwd.join("panel.toml"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        tried.push(dir.join("panel.toml"));
    }
    tried.push("/etc/panel.toml".into());
    tried.dedup(); // 直接在部署目录里跑的时候，前两条是同一个路径

    if let Some(found) = tried.iter().find(|p| p.is_file()) {
        return Ok(found.clone());
    }
    let list: Vec<String> = tried.iter().map(|p| format!("  {}", p.display())).collect();
    Err(format!(
        "找不到配置文件，这几个位置都看过了：\n{}\n\
         照 panel.toml.example 改一份，放到上面任意一个位置；\
         或者用 PANEL_CONFIG=/你的/路径 指定",
        list.join("\n")
    )
    .into())
}
