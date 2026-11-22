mod common;

use std::{
    error::Error,
    fs,
    path::Path,
    time::Duration,
};

use rstest::rstest;
use url::Url;

/// The longest that loading a small workspace can take
/// without a loop.
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Writes a workspace root that lists `members`, with a
/// member manifest at each `(dir, text)` of `extra`.
fn write_workspace(
    root: &Path,
    members: &str,
    extra: &[(&str, &str)],
) -> std::io::Result<()>
{
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = [{members}]\n\n[package]\nname = \"root\"\nversion = \
             \"0.1.0\"\n"
        ),
    )?;
    for (dir, text) in extra
    {
        fs::create_dir_all(root.join(dir))?;
        fs::write(root.join(dir).join("Cargo.toml"), text)?;
    }
    Ok(())
}

// frederik-uni/zed-cargotom#25: a workspace whose members
// resolve back to its own manifest made the load recurse
// forever.
#[rstest]
#[case::dot_member("\".\"", &[])]
#[case::member_aliases_root("\"a\"", &[("a", "[workspace]\nmembers = [\"..\"]\n")])]
#[tokio::test]
async fn self_referential_workspace_loads(
    #[case] members: &str,
    #[case] extra: &[(&str, &str)],
) -> Result<(), Box<dyn Error>>
{
    let dir = tempfile::tempdir()?;
    write_workspace(dir.path(), members, extra)?;
    assert_loads(dir.path()).await
}

// frederik-uni/zed-cargotom#25: a member symlinked to the
// root has a different address for the same manifest.
#[cfg(unix)]
#[tokio::test]
async fn symlinked_member_loads() -> Result<(), Box<dyn Error>>
{
    let dir = tempfile::tempdir()?;
    write_workspace(dir.path(), "\"a\"", &[])?;
    std::os::unix::fs::symlink(dir.path(), dir.path().join("a"))?;
    assert_loads(dir.path()).await
}

/// Loads the root manifest in `root` and checks that it
/// parses in time.
async fn assert_loads(root: &Path) -> Result<(), Box<dyn Error>>
{
    let uri = Url::from_file_path(root.join("Cargo.toml")).map_err(|()| "not absolute")?;
    let db = common::offline_db(root);
    tokio::time::timeout(LOAD_TIMEOUT, async {
        db.write().await.try_init(&uri).await
    })
    .await??;
    let lock = db.read().await;
    let toml = lock.get_toml(&uri).ok_or("root not loaded")?;
    assert!(toml.workspace);
    Ok(())
}
