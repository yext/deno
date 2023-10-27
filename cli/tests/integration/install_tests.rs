// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::fs;
use std::process::Command;
use test_util as util;
use test_util::assert_contains;
use test_util::assert_ends_with;
use test_util::TempDir;

#[test]
fn install_basic() {
  let _guard = util::http_server();
  let temp_dir = TempDir::new();
  let temp_dir_str = temp_dir.path().to_string();

  // ensure a lockfile doesn't get created or updated locally
  temp_dir.write("deno.json", "{}");

  let status = util::deno_cmd()
    .current_dir(temp_dir.path())
    .arg("install")
    .arg("--check")
    .arg("--name")
    .arg("echo_test")
    .arg("http://localhost:4545/echo.ts")
    .envs([
      ("HOME", temp_dir_str.as_str()),
      ("USERPROFILE", temp_dir_str.as_str()),
      ("DENO_INSTALL_ROOT", ""),
    ])
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
  assert!(status.success());

  // no lockfile should be created locally
  assert!(!temp_dir.path().join("deno.lock").exists());

  let mut file_path = temp_dir.path().join(".deno/bin/echo_test");
  assert!(file_path.exists());

  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }

  let content = file_path.read_to_string();
  // ensure there's a trailing newline so the shell script can be
  // more versatile.
  assert_eq!(content.chars().last().unwrap(), '\n');

  if cfg!(windows) {
    assert_contains!(
      content,
      r#""run" "--check" "--no-config" "http://localhost:4545/echo.ts""#
    );
  } else {
    assert_contains!(
      content,
      r#"run --check --no-config 'http://localhost:4545/echo.ts'"#
    );
  }

  // now uninstall
  let status = util::deno_cmd()
    .current_dir(temp_dir.path())
    .arg("uninstall")
    .arg("echo_test")
    .envs([
      ("HOME", temp_dir_str.as_str()),
      ("USERPROFILE", temp_dir_str.as_str()),
      ("DENO_INSTALL_ROOT", ""),
    ])
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
  assert!(status.success());

  // ensure local lockfile still doesn't exist
  assert!(!temp_dir.path().join("deno.lock").exists());
  // ensure uninstall occurred
  assert!(!file_path.exists());
}

#[test]
fn install_custom_dir_env_var() {
  let _guard = util::http_server();
  let temp_dir = TempDir::new();
  let temp_dir_str = temp_dir.path().to_string();

  let status = util::deno_cmd()
    .current_dir(util::root_path()) // different cwd
    .arg("install")
    .arg("--check")
    .arg("--name")
    .arg("echo_test")
    .arg("http://localhost:4545/echo.ts")
    .envs([
      ("HOME", temp_dir_str.as_str()),
      ("USERPROFILE", temp_dir_str.as_str()),
      ("DENO_INSTALL_ROOT", temp_dir_str.as_str()),
    ])
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
  assert!(status.success());

  let mut file_path = temp_dir.path().join("bin/echo_test");
  assert!(file_path.exists());

  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }

  let content = fs::read_to_string(file_path).unwrap();
  if cfg!(windows) {
    assert_contains!(
      content,
      r#""run" "--check" "--no-config" "http://localhost:4545/echo.ts""#
    );
  } else {
    assert_contains!(
      content,
      r#"run --check --no-config 'http://localhost:4545/echo.ts'"#
    );
  }
}

#[test]
fn installer_test_local_module_run() {
  let temp_dir = TempDir::new();
  let bin_dir = temp_dir.path().join("bin");
  std::fs::create_dir(&bin_dir).unwrap();
  let status = util::deno_cmd()
    .current_dir(util::root_path())
    .arg("install")
    .arg("--name")
    .arg("echo_test")
    .arg("--root")
    .arg(temp_dir.path())
    .arg(util::testdata_path().join("echo.ts"))
    .arg("hello")
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
  assert!(status.success());
  let mut file_path = bin_dir.join("echo_test");
  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }
  assert!(file_path.exists());
  // NOTE: using file_path here instead of exec_name, because tests
  // shouldn't mess with user's PATH env variable
  let output = Command::new(file_path)
    .current_dir(temp_dir.path())
    .arg("foo")
    .env("PATH", util::target_dir())
    .output()
    .unwrap();
  let stdout_str = std::str::from_utf8(&output.stdout).unwrap().trim();
  assert_ends_with!(stdout_str, "hello, foo");
}

#[test]
fn installer_test_remote_module_run() {
  let _g = util::http_server();
  let temp_dir = TempDir::new();
  let bin_dir = temp_dir.path().join("bin");
  std::fs::create_dir(&bin_dir).unwrap();
  let status = util::deno_cmd()
    .current_dir(util::testdata_path())
    .arg("install")
    .arg("--name")
    .arg("echo_test")
    .arg("--root")
    .arg(temp_dir.path())
    .arg("http://localhost:4545/echo.ts")
    .arg("hello")
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
  assert!(status.success());
  let mut file_path = bin_dir.join("echo_test");
  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }
  assert!(file_path.exists());
  let output = Command::new(file_path)
    .current_dir(temp_dir.path())
    .arg("foo")
    .env("PATH", util::target_dir())
    .output()
    .unwrap();
  assert_ends_with!(
    std::str::from_utf8(&output.stdout).unwrap().trim(),
    "hello, foo",
  );
}

#[test]
fn check_local_by_default() {
  let _guard = util::http_server();
  let temp_dir = TempDir::new();
  let temp_dir_str = temp_dir.path().to_string();

  let status = util::deno_cmd()
    .current_dir(temp_dir.path())
    .arg("install")
    .arg(util::testdata_path().join("./install/check_local_by_default.ts"))
    .envs([
      ("HOME", temp_dir_str.as_str()),
      ("USERPROFILE", temp_dir_str.as_str()),
      ("DENO_INSTALL_ROOT", ""),
    ])
    .status()
    .unwrap();
  assert!(status.success());
}

#[test]
fn check_local_by_default2() {
  let _guard = util::http_server();
  let temp_dir = TempDir::new();
  let temp_dir_str = temp_dir.path().to_string();

  let status = util::deno_cmd()
    .current_dir(temp_dir.path())
    .arg("install")
    .arg(util::testdata_path().join("./install/check_local_by_default2.ts"))
    .envs([
      ("HOME", temp_dir_str.as_str()),
      ("NO_COLOR", "1"),
      ("USERPROFILE", temp_dir_str.as_str()),
      ("DENO_INSTALL_ROOT", ""),
    ])
    .status()
    .unwrap();
  assert!(status.success());
}
