# Solexa

**Canceled.** I started this at the beginning of 2026 while I had no internet. Living in Iran, the internet was shut down on people because of the war and the Islamic Republic's crimes against humanity. Now at the end of 2026 I don't see a future for this app anymore. Better to stop than to push blindly with no goal.

I'm sorry <3.

The code is here as I left it. If any part of it is useful to you, take it.

## What it was

A desktop app for running a small team of AI agents on a coding project. You give it a task and the agents plan, write, verify and hand work to each other. It ran locally and talked to local model servers (llama.cpp, Ollama) so it could work without internet. Project state lived in SQLite.

Rust and Tauri on the backend, React and TypeScript on the frontend. The core is split into crates: domain, models, coordinator, storage, sync, github, verification. The desktop app is a thin shell around them.

## Screenshots

<div align="center">
  <table>
    <tr>
      <td align="center" width="50%">
        <img src="screenshots/main-top.png" alt="Main workspace top" width="100%" />
        <br />
        <sub>Main workspace: projects, team and tasks</sub>
      </td>
      <td align="center" width="50%">
        <img src="screenshots/main-bottom.png" alt="Main workspace bottom" width="100%" />
        <br />
        <sub>Main workspace: requests, rules and resource limits</sub>
      </td>
    </tr>
    <tr>
      <td align="center" width="50%">
        <img src="screenshots/settings-general.png" alt="Settings general" width="100%" />
        <br />
        <sub>Settings: language, system tray and startup</sub>
      </td>
      <td align="center" width="50%">
        <img src="screenshots/settings-engine.png" alt="Settings engine" width="100%" />
        <br />
        <sub>Settings: engine limits and approval rules</sub>
      </td>
    </tr>
    <tr>
      <td align="center" width="50%">
        <img src="screenshots/settings-appearance.png" alt="Settings appearance" width="100%" />
        <br />
        <sub>Settings: theme studio and color preview</sub>
      </td>
      <td align="center" width="50%">
      </td>
    </tr>
  </table>
</div>

## Installers

Windows only. Nothing for Linux or macOS.

- `SOLEXA_0.1.0_x64-setup.exe` (NSIS installer)
- `SOLEXA_0.1.0_x64_en-US.msi` (MSI installer)

Both were built and tested on Windows. They install and run. That is all the testing I did on them.

## Build from source

You need Rust, Node and the Tauri prerequisites for your OS.

Backend and crates, from the repo root:

```
cargo build --release
```

Frontend, inside `apps/desktop/frontend`:

```
npm install
npm run build
```

The Tauri app lives in `apps/desktop/src-tauri` and expects the frontend build output.

## Tests

There were 363 tests passing when I stopped. They cover domain logic, coordination, storage, sync and verification. I can't promise they still pass on your machine. They were green on mine.

## License

MIT License.
See the `LICENSE` file.
