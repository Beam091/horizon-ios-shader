# Horizon on iPhone and iPad (with Java Shader Pack Support)

An iOS port of Horizon (Minecraft: Java Edition 26.3 in Rust) combining:
- **`horizon-rust/horizon`**: upstream `horizon-shaders`, `naga` (GLSL → WGSL → Metal), and `wgpu-hal` Metal fixes.
- **`meehandaniel4/horizon-iOS`**: touch controls, safe-area layout editor, UIKit keyboard & clipboard, 64 MB `BigStack` event loop, and in-app **Get mods** browser.
- **`PommeMC/Client`**: in-app Microsoft OAuth2 Device Code sign-in (online-mode servers & Realms), Iris `LegacyIdMap` / `block.properties` compatibility fixes, and iCloud backup exclusion for `resources/`.

## Features on iOS

- **Java Edition Shader Packs on Metal**: Put Java Edition OptiFine/Iris shader packs (`.zip` or folder) into **On My iPhone → Horizon → shaderpacks**, or install them directly from **Get mods → Shader packs** on the title screen, then select them in **Options → Video Settings → Shader Packs**. Supports `block.properties` (with `%tag` block tags, Iris line-continuation/inline-key fix-ups, and `LegacyIdMap` fallback for older packs), LabPBR paged maps, indirect compute dispatches, file-backed SSBOs, and Metal storage-image stand-ins (`R11F_G11F_B10F` → `Rgba16Float`).
- **Microsoft Account Sign-In**: Tap **Sign in** (top right of the title screen, beside **Get mods**) to sign in with Microsoft's Device Code flow. Horizon copies the sign-in code to the clipboard, opens `microsoft.com/link` in Safari, exchanges the token via Xbox Live/XSTS, and automatically refreshes your session on future launches so you can join online-mode servers and Realms without a PC launcher.
- **Touch Controls & Layout Editor**: Left analog movement stick (push forward to sprint), drag to look, tap to use/place, hold to mine/attack, customizable HUD buttons (jump, sneak, hit, use, drop, chat, view, pause, inventory, F3, Esc), and hotbar taps. Tap the pencil button (top right) to drag, resize, change opacity, or hide controls (saved in `touch-controls.txt`).
- **Bluetooth Controllers (`GameController.framework`)**: Native iOS support for Xbox, DualSense, DualShock 4, Nintendo Switch Pro, and MFi controllers paired in iOS Settings, with automatic button glyph detection.
- **Native Keyboard & Clipboard**: UIKit on-screen keyboard (`UIKeyboardWillChangeFrameNotification`) lifts chat/text fields automatically, plus `UIPasteboard` copy/paste support.
- **In-App Resource Import & iCloud Protection**: Place `horizon-resources.zip` directly in **On My iPhone → Horizon** and tap **Check again** (no need to unzip in the Files app), or copy an `https://` link to your prepared archive and tap **Download from copied link**. The `resources/` folder is automatically excluded from iCloud backups (`NSURLIsExcludedFromBackupKey`).
- **iOS Memory Tuning**: Starts with mobile-friendly defaults (`renderDistance: 8`, `simulationDistance: 6`) and releases unused allocator pages immediately on iOS low-memory warnings (`memory_warning` → `release_free_memory`).

## Get or Build the `.ipa`

- **GitHub Actions (macOS)**: `.github/workflows/ios-build.yml` builds an unsigned `Horizon.ipa` on GitHub's macOS runners.
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
