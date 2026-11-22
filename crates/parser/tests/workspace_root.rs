mod common;

use std::{
    error::Error,
    fs,
};

use url::Url;

// A member matched by a glob, opened before its root, must
// still resolve the root so that `workspace = true`
// dependencies have a source.
#[tokio::test]
async fn member_opened_first_finds_glob_root() -> Result<(), Box<dyn Error>>
{
    let dir = tempfile::tempdir()?;
    let member = dir.path().join("crates").join("a");
    fs::create_dir_all(member.join("src"))?;
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
    )?;
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde.workspace = true\n",
    )?;
    fs::write(member.join("src").join("lib.rs"), "")?;
    let member_uri = Url::from_file_path(member.join("Cargo.toml")).map_err(|()| "not absolute")?;
    let root_uri =
        Url::from_file_path(dir.path().join("Cargo.toml")).map_err(|()| "not absolute")?;
    let db = common::offline_db(dir.path());

    db.write().await.try_init(&member_uri).await?;

    let lock = db.read().await;
    assert_eq!(lock.get_workspace(&member_uri), Some(&root_uri));
    let root = lock.get_toml(&root_uri).ok_or("root not loaded")?;
    assert!(root.workspace);
    assert_eq!(
        root.dependencies.first().map(|v| v.data.name()),
        Some("serde")
    );
    Ok(())
}
