# Horizon on iPhone and iPad (with Java Shader Pack Support)

An iOS port of Horizon (Minecraft: Java Edition 26.3 in Rust) combining:
- **`horizon-rust/horizon`**: upstream `horizon-shaders`, `naga` (GLSL → WGSL → Metal), and `wgpu-hal` Metal fixes.
- **`meehandaniel4/horizon-iOS`**: touch controls, safe-area layout editor, UIKit keyboard & clipboard, 64 MB `BigStack` event loop, and in-app **Get mods** browser.
- **`PommeMC/Client`**: in-app Microsoft OAuth2 Device Code sign-in (online-mode servers & Realms), Iris `LegacyIdMap` / `block.properties` compatibility fixes, and iCloud backup exclusion for `resources/`.

## Features on iOS (v0.3.0)

- **Java Edition Shader Packs on Metal**: Put Java Edition OptiFine/Iris shader packs (`.zip` or folder) into **On My iPhone → Horizon → shaderpacks**, or install them directly from **Get mods → Shader packs** on the title screen, then select them in **Options → Video Settings → Shader Packs**. Supports `block.properties` (with `%tag` block tags, Iris line-continuation/inline-key fix-ups, and `LegacyIdMap` fallback), LabPBR paged maps, indirect compute dispatches, file-backed SSBOs, 16-bit shader buffers, shadow caching & resolution cap, and stable Safe Metal shader compilation.
- **MetalFX Upscaling & Frame Generation**: Hardware Apple **MetalFX Spatial & Temporal Upscaling** (`2x`/`3x` with sharp native HUD) plus fast WGSL motion-compensated **2x Frame Generation** (e.g. 30 real FPS → 60 display FPS) for both Vanilla and Shader Packs.
- **iPad Hardware Keyboard & Mouse (Native Pointer Lock)**: Full `UIScene` lifecycle (`Xcode 27 / iOS 27 SDK`) with native iPadOS `prefersPointerLocked` mouse capture in-world, 1:1 native pointer hover in menus, hardware keyboard gameplay & editing shortcuts (`Cmd`/`Ctrl`+`A`/`C`/`V`/`X`, continuous hold-to-delete `Backspace`/`Delete`, `Arrow` keys), and dedicated Hardware Mouse & Hotbar Scroll sensitivity sliders.
- **Offline & Microsoft Accounts**: Tap the account button on the title screen (beside **Get mods**) to either set/switch an **Offline Account** username (for Singleplayer, LAN, and offline-mode servers) or sign in with your **Microsoft Account** (Device Code flow with auto-refresh for online-mode servers and Realms).
- **Touch Controls & Layout Editor**: Left analog movement stick (push forward to sprint), drag to look, tap to use/place, hold to mine/attack, Touch Zoom, customizable HUD buttons, long-press Paste, and full Thai (`th_th`) & English (`en_us`) localization. Tap the pencil button (top right) to drag, resize, change opacity, or hide controls (saved in `touch-controls.txt`).
- **Bluetooth Controllers (`GameController.framework`)**: Native iOS support for Xbox, DualSense, DualShock 4, Nintendo Switch Pro, and MFi controllers paired in iOS Settings, with automatic button glyph detection.
- **Native Keyboard & Clipboard**: UIKit on-screen keyboard (`UIKeyboardWillChangeFrameNotification`) lifts chat/text fields automatically, plus `UIPasteboard` copy/paste support.
- **In-App Resource Import & iCloud Protection**: Place `horizon-resources.zip` directly in **On My iPhone → Horizon** and tap **Check again** (no need to unzip in the Files app), or copy an `https://` link to your prepared archive and tap **Download from copied link**. The `resources/` folder is automatically excluded from iCloud backups (`NSURLIsExcludedFromBackupKey`).
- **iOS Memory Tuning**: Starts with mobile-friendly defaults (`renderDistance: 8`, `simulationDistance: 6`), frees unused MetalFX/Frame Gen VRAM immediately when toggled off, and releases allocator caches on iOS low-memory warnings (`memory_warning`).

## Get or Build the `.ipa`

- **GitHub Actions (macOS `xcode-27`)**: `.github/workflows/ios-build.yml` builds an unsigned `Horizon.ipa` on GitHub's Apple Silicon macOS runners and publishes `Horizon-ios-<version>-b<N>.ipa`.
- **Local macOS or Linux cross-build**: `packaging/ios/package.sh BINARY VERSION OUTDIR` works on both macOS (`lipo`, `sips`, `plutil`, `ditto`) and Linux (`od`, `python3` + Pillow, `plistlib`, `zip`).

## Install `Horizon.ipa` (Sideloadly / AltStore / SideStore)

1. Install [Sideloadly](https://sideloadly.io) (Windows/macOS), [AltStore](https://altstore.io), or [SideStore](https://sidestore.io).
2. Connect your iPhone/iPad (or use SideStore on-device), drop `Horizon.ipa` into the sideloader, and sign with your Apple ID.
3. On iPhone/iPad: **Settings → General → VPN & Device Management** → trust your Apple ID.
   On iOS 16+: enable **Settings → Privacy & Security → Developer Mode**.

With a free Apple ID, re-sign every 7 days (your worlds, shaders, and sign-in stay intact).

## Provide Your Game Resources

Horizon ships none of Mojang's copyrighted files. Prepare them once from your own copy of Minecraft: Java Edition 26.3:

- **Option A — On a PC**: Run the Horizon launcher or `horizon-prepare`, then copy `resources/` (or `horizon-resources.zip`) into **On My iPhone → Horizon**.
- **Option B — Using only your iPhone/iPad (via a private GitHub repo)**:
  1. Create a **private** GitHub repository and add `packaging/ios/prepare/prepare-game-files.yml` as `.github/workflows/prepare-game-files.yml`.
  2. Open **Actions → Prepare game files → Run workflow**, confirm you own Java Edition, and run it.
  3. Either download `horizon-resources.zip` into **On My iPhone → Horizon** and tap **Check again** inside Horizon, **or** copy the `https://` download link and tap **Download from copied link** inside Horizon.
  4. Once unpacked, restart Horizon to play.

## Remaining Limitations

- **Native Rust mods**: Compiled Rust mods cannot be compiled on-device due to iOS code-signing restrictions; use the companion GitHub Actions workflow (`packaging/ios/native-mods.md`) to build a custom `.ipa` with native mods baked in.
- **Java-based resource preparation**: Generating `horizon-resources.json` from Mojang's client JAR still requires `horizon-prepare` (on a PC or private GitHub Actions workflow).
