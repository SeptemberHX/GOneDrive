# Building and development

**English** | [简体中文](BUILDING.zh-CN.md)

> Most users should install the `.deb` from [Releases](https://github.com/SeptemberHX/GOneDrive/releases) as described in the [README](../README.md). This page is for people who build, package or work on GOnedrive themselves.

## Build

```bash
# Dependencies (Ubuntu / Debian)
sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential

# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

cargo build --release
```

Once you have a client id (next section), build distributable binaries with:

```bash
GONEDRIVE_CLIENT_ID=<your id> cargo build --release
```

SQLite and TLS need no system development packages: `rusqlite` compiles SQLite statically and TLS uses rustls.

Outputs:

| Binary | Role |
|---|---|
| `target/release/gonedrived` | Background sync daemon (runs as the user) |
| `target/release/gonedrive` | GTK 4 panel and tray (a pure D-Bus client; holds no credentials) |
| `target/release/gonedrive-hsm` | Files-on-demand helper (runs as root; no network, no credentials) |

---

## Registering an Entra ID application (once, about 3 minutes)

**A `client_id` is not your account.** You still sign in with your own Microsoft account on Microsoft's own page. The `client_id` only identifies *which program* is asking for access. It is not a secret, and Microsoft's OAuth has no registration-free path (ROPC is disabled for personal accounts and for accounts with MFA).

**This is done once by whoever builds the package, not by every user.** It is the same approach as abraunegg/onedrive, rclone and onedriver: register one application, bake its id into the binaries, and every user can sign in out of the box. Anyone who wants their own API quota can override it.

### Precedence

| Source | Use |
|---|---|
| Environment variable `GONEDRIVE_CLIENT_ID` | Overrides everything; for development and quick tests |
| `client_id` in the config file | Per-user override (for example, if the shared registration is throttled) |
| Baked in at build time | **The recommended way to distribute**, see below |

If none is set, the daemon refuses to start and prints full instructions, rather than failing later at Microsoft with an unexplained `AADSTS700016`.

### Steps

You need an Entra tenant. **A personal outlook.com account does not have one by default.** If you have a work or school Microsoft 365 account, sign in with that (the application it registers can still sign in to personal OneDrive accounts). Otherwise create a free account at [azure.microsoft.com/free](https://azure.microsoft.com/free), which creates a Default Directory for you.

1. Open [Entra ID portal → App registrations → New registration](https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade).
2. **Name**: anything, for example `GOnedrive`.
3. **Supported account types**: **"Accounts in any organizational directory and personal Microsoft accounts"**. Without the "personal" part, outlook.com accounts are refused.
4. **Redirect URI**: choose **"Public client/native (mobile & desktop)"** and enter **`http://localhost`**.
5. After creating it, copy the **"Application (client) ID"** from the overview page.

> **Enter `http://localhost` exactly, with no path and no port.**
> Microsoft ignores the port when matching `localhost` redirect URIs (which is what lets this client use a random port), but **the path must match exactly**. Registering `http://localhost/callback` while the client requests `http://localhost:1234` fails with `AADSTS50011: reply URL does not match`.
> This is also Microsoft's recommended value for desktop apps that use the system browser.
>
> Microsoft's documentation prefers `127.0.0.1` over `localhost`, but the portal refuses `http` + `127.0.0.1` (it needs a manual manifest edit), and the port exemption is written for `localhost`.

### Using it

```bash
# For distribution: bake it in so users never deal with it
GONEDRIVE_CLIENT_ID=<your id> cargo build --release

# Or just to try it out
export GONEDRIVE_CLIENT_ID=<your id>
```

Then `gonedrived login` opens the browser; sign in with **your own Microsoft account** and grant consent. The refresh token is stored in the system keyring, never in the database.

---

## Installing a build

Two paths. **Pick one**; do not mix them.

**1. Debian package (recommended for everyday use)**

```bash
GONEDRIVE_CLIENT_ID=<your app id> ./build-deb.sh
sudo apt install ./gonedrive_*.deb
```

The binaries go to `/usr/bin`, the package registers the helper and the user service itself, and upgrading is just installing a newer package.

**2. From source (recommended for development)**

```bash
sudo ./install.sh     # binaries + the privileged helper system service (the only part needing root)
./install.sh --user   # daemon user service + Nautilus integration + icons
```

This installs to `/usr/local/bin` and `$HOME`, both of which take precedence over the package's files. If you run `install.sh` after installing the deb, later package upgrades "install but don't run": the old binaries keep running and nothing in the UI shows it. `install.sh` therefore refuses to run when the deb is installed; set `GONEDRIVE_ALLOW_BESIDE_DEB=1` if you really need both.

To switch from a source install to the package, remove the source install first:

```bash
sudo rm /etc/systemd/system/gonedrive-hsm.service \
        /usr/local/bin/gonedrive /usr/local/bin/gonedrived /usr/local/bin/gonedrive-hsm
sudo systemctl daemon-reload
```

---

## Tests

```bash
cargo test --workspace
# The GTK tests need a display; CI runs them as:
CI=true xvfb-run -a dbus-run-session -- cargo test --workspace
python3 -m unittest discover -s data/nautilus/tests
```

End-to-end integration tests start a fake Microsoft Graph with `wiremock` and drive the real engine, database and filesystem. They cover lazy first sign-in, the Partial/Populated directory boundary, children/delta ordering, incremental sync, remote deletes, local additions, two-sided conflicts, per-item deferral, 410 resync, 429 back-off, and data-safety guards such as "a pending download must never be mistaken for a local delete".

Unit tests cover every cell of the conflict matrix, delta topological sorting, path derivation, case collisions, upload chunk boundaries (4 MiB threshold, 320 KiB multiples), and OAuth loopback state validation and error propagation.

## Debugging aids

- `GONEDRIVE_PAGE=activity|folders|settings` opens the panel on that page.
- `GONEDRIVE_NO_TRAY=1` starts without the tray icon.
- `GONEDRIVE_SNAPSHOT=/path.png` renders the window offscreen and exits. The offscreen render currently drops most text labels, so it is useful for layout checks only, not for screenshots.
- `RUST_LOG=debug` on either service enables detailed logs; see [the logging section of the design notes](DESIGN.md#按需开启诊断日志) (Chinese).

## Continuous integration and releases

`.github/workflows/ci.yml` runs on every push and pull request: `cargo fmt --check`, `clippy -D warnings`, the full test suite (GTK tests under xvfb), the Nautilus extension tests and `cargo audit`. It uploads the `.deb` as a build artifact.

To publish a release:

1. Bump `version` in `Cargo.toml` and commit.
2. Push a matching tag: `git tag v0.1.7 && git push origin v0.1.7`.
3. CI checks that the tag matches the version, then creates a GitHub Release with the `.deb` and `SHA256SUMS` attached.

So that released packages can sign in out of the box, add `GONEDRIVE_CLIENT_ID` under the repository's Settings → Secrets and variables → Actions → **Variables**. It is public configuration, not a secret, so use a variable rather than a secret.
