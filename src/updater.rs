//! Обновление через GitHub Releases.
//!
//! Приложение не нотаризовано Apple, поэтому образ, скачанный браузером,
//! получает флаг карантина и его приходится снимать руками. Скачанный самим
//! приложением — не получает: карантин ставит загрузчик, а не система.
//! Поэтому автообновление избавляет от ручного шага навсегда после первой
//! установки.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "davnozdu/LLM-dict";

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub dmg_url: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("LLM-dict/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// Сравнение версий вида 1.2.3. Незнакомые куски считаем нулями,
/// чтобы кривой тег не превращался в «обновление есть всегда».
fn parse_version(v: &str) -> (u32, u32, u32) {
    let v = v.trim_start_matches('v');
    let mut it = v
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Возвращает релиз, если он новее текущего.
pub fn check() -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = client()?.get(&url).send()?;
    if !resp.status().is_success() {
        bail!("GitHub ответил {}", resp.status().as_u16());
    }
    let release: ApiRelease = resp.json()?;
    if release.prerelease {
        return Ok(None);
    }

    if parse_version(&release.tag_name) <= parse_version(current_version()) {
        return Ok(None);
    }

    let dmg = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".dmg"))
        .ok_or_else(|| anyhow!("в релизе {} нет образа .dmg", release.tag_name))?;

    Ok(Some(Release {
        version: release.tag_name.trim_start_matches('v').to_string(),
        notes: release.body,
        dmg_url: dmg.browser_download_url.clone(),
    }))
}

/// Путь к .app, внутри которого мы запущены.
fn current_app_bundle() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    // .../LLM-dict.app/Contents/MacOS/llm-dict
    let app = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("не определить путь к бандлу"))?;
    if app.extension().and_then(|e| e.to_str()) != Some("app") {
        bail!("приложение запущено не из бандла .app — обновление недоступно");
    }
    Ok(app.to_path_buf())
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        bail!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Скачивает образ, проверяет подпись нового бандла и подменяет текущий.
/// Возвращает путь к обновлённому приложению.
pub fn install(release: &Release) -> Result<PathBuf> {
    let target = current_app_bundle()?;

    let work_dir = tempfile::Builder::new()
        .prefix("llm-dict-update-")
        .tempdir()?;
    let work = work_dir.path();

    let dmg_path = work.join("update.dmg");
    let bytes = client()?
        .get(&release.dmg_url)
        .send()?
        .error_for_status()?
        .bytes()?;
    std::fs::write(&dmg_path, &bytes).context("сохранить образ")?;

    let mount = work.join("mnt");
    std::fs::create_dir_all(&mount)?;
    run(
        "/usr/bin/hdiutil",
        &[
            "attach",
            &dmg_path.to_string_lossy(),
            "-nobrowse",
            "-readonly",
            "-mountpoint",
            &mount.to_string_lossy(),
        ],
    )
    .context("смонтировать образ")?;

    let result = install_from_mount(&mount, &target);

    let _ = run("/usr/bin/hdiutil", &["detach", &mount.to_string_lossy()]);
    let _ = std::fs::remove_file(&dmg_path);

    result.map(|()| target)
}

fn install_from_mount(mount: &Path, target: &Path) -> Result<()> {
    let new_app = mount.join("LLM-dict.app");
    if !new_app.exists() {
        bail!("в образе нет LLM-dict.app");
    }

    // Подпись проверяем до подмены: битую или подделанную сборку ставить нельзя.
    run(
        "/usr/bin/codesign",
        &["--verify", "--deep", &new_app.to_string_lossy()],
    )
    .context("новая сборка не проходит проверку подписи")?;

    // Staging на том же томе: rename между томами не работает.
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("нет каталога приложения"))?;
    let staging = tempfile::Builder::new()
        .prefix(".llm-dict-update-")
        .tempdir_in(parent)?;
    let work = staging.path();
    verify_identity(target, &new_app, work)?;
    let staged = work.join("LLM-dict.app");
    run(
        "/usr/bin/ditto",
        &[&new_app.to_string_lossy(), &staged.to_string_lossy()],
    )
    .context("скопировать новую сборку")?;

    // Карантина на скачанном нами файле нет, но подстрахуемся: если образ
    // когда-то придёт с флагом, приложение молча перестанет запускаться.
    let _ = run(
        "/usr/bin/xattr",
        &["-dr", "com.apple.quarantine", &staged.to_string_lossy()],
    );

    let backup = work.join("LLM-dict.app.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(target, &backup).context("отодвинуть текущую версию")?;
    if let Err(e) = std::fs::rename(&staged, target) {
        // Откатываемся, чтобы не остаться вообще без приложения.
        if let Err(rollback) = std::fs::rename(&backup, target) {
            let saved = staging.keep();
            bail!("не поставить новую версию: {e}; не вернуть прежнюю: {rollback}. Резервная копия: {}", saved.join("LLM-dict.app.old").display());
        }
        return Err(e).context("поставить новую версию");
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

/// Сравниваем DER самого сертификата, а не отображаемое имя его владельца.
fn verify_identity(current: &Path, candidate: &Path, work: &Path) -> Result<()> {
    let ours = signing_certificate(current, &work.join("current-cert"))?;
    let theirs = signing_certificate(candidate, &work.join("new-cert"))?;
    if ours != theirs {
        bail!("новая сборка подписана другим сертификатом, установка отменена");
    }
    for app in [current, candidate] {
        let output = std::process::Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=2"])
            .arg(app)
            .output()?;
        if !output.status.success() {
            bail!("не прочитать идентификатор подписанного приложения");
        }
        let text = String::from_utf8_lossy(&output.stderr);
        if !text
            .lines()
            .any(|line| line == "Identifier=com.davnozdu.llm-dict")
        {
            bail!("неверный идентификатор подписанного приложения");
        }
    }
    Ok(())
}

fn signing_certificate(app: &Path, prefix: &Path) -> Result<Vec<u8>> {
    run(
        "/usr/bin/codesign",
        &[
            "-d",
            "--extract-certificates",
            &prefix.to_string_lossy(),
            &app.to_string_lossy(),
        ],
    )
    .context("не извлечь сертификат подписи")?;
    let certificate = std::fs::read(format!("{}0", prefix.display()))
        .context("нет сертификата подписи; для ad-hoc сборки установите обновление вручную")?;
    if certificate.is_empty() {
        bail!("пустой сертификат подписи");
    }
    Ok(certificate)
}

/// Перезапускает приложение и завершает текущий процесс.
pub fn relaunch(app: &Path) -> ! {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app)
        .spawn();
    std::thread::sleep(Duration::from_millis(300));
    std::process::exit(0);
}
