# cdp-fs: Chrome DevTools Protocol FUSE Filesystem

`cdp-fs` is a high-performance FUSE filesystem written in Rust that mounts running Chrome and Chromium browser tabs as an interactive, traversable POSIX folder hierarchy by connecting directly to the Chrome DevTools Protocol (CDP) debugging endpoint (`http://localhost:9222`).

Unlike [TabFS](https://omar.website/tabfs/), `cdp-fs` requires **no browser extension** and **no native messaging host**, making it immune to Chrome extension API changes (such as the deprecation of Manifest V2).

---

## Features

- **Direct CDP Communication**: Interacts directly with Chrome/Chromium via HTTP and WebSocket DevTools Protocol endpoints.
- **Tab Titles as Folders**: Each tab's directory is named after its page title (with sanitization for `/` and control characters, and automatic numeric disambiguation for duplicates).
- **Interactive POSIX File Interface**:
  - `url`: Read tab URL; write to navigate to a new URL.
  - `title`: Read document title; write to dynamically set `document.title`.
  - `text`: Read document `innerText` (human-readable plain text).
  - `html`: Read document `outerHTML`; write to update `document.documentElement.innerHTML`.
  - `eval`: Write any JavaScript expression to execute it inside the tab; read to inspect the result of the last evaluation.
  - `screenshot.png`: Read to capture a live PNG screenshot of the tab.
  - `id`: Read the CDP target ID string.
  - `reload`: Touch or write to trigger page reload.
  - `activate`: Touch or write to bring the tab into focus in Chrome.
  - `close`: Touch or write to close the tab.
- **POSIX Directory Operations**:
  - `rm -rf /mountpoint/tab-name` or `rmdir /mountpoint/tab-name`: Closes the browser tab in Chrome!
  - `mkdir /mountpoint/new-tab` or `mkdir /mountpoint/https://example.com`: Creates and opens a new browser tab.
- **Robust Error & Lifecycle Handling**:
  - Gracefully mounts even if Chrome/Chromium is not yet running (directories populate automatically when Chrome starts).
  - Handles tabs closing while open files or directories are being read without crashing or hanging (returns clean POSIX error codes like `ENOENT`).

---

## Directory Hierarchy Example

```text
/mountpoint/
  ├── Example Domain/
  │     ├── url            # Read: tab URL. Write: navigate to new URL.
  │     ├── title          # Read: document title. Write: set title.
  │     ├── text           # Read: plain text content (document.body.innerText).
  │     ├── html           # Read: outer HTML. Write: innerHTML.
  │     ├── eval           # Read: last eval result. Write: evaluate JS code.
  │     ├── screenshot.png # Read: captures a live PNG screenshot of the tab.
  │     ├── id             # Read: the CDP target ID.
  │     ├── reload         # Write/Touch: reload tab.
  │     ├── activate       # Write/Touch: focus tab in Chrome.
  │     └── close          # Write/Touch: close tab.
  └── GitHub/
        ...
```

---

## Prerequisites

1. **Rust & Cargo** (1.75+ or newer).
2. **Chrome or Chromium** with remote debugging enabled (`--remote-debugging-port=9222`).
3. **FUSE 3** installed on Linux (`/dev/fuse` permissions).

---

## Building

```bash
cargo build --release
```

The resulting binary will be located at `target/release/cdp-fs`.

---

## Usage

### 1. Launch Chrome / Chromium with Remote Debugging

```bash
# On Linux (Google Chrome):
google-chrome --remote-debugging-port=9222

# Or Chromium:
chromium --remote-debugging-port=9222
```

### 2. Mount `cdp-fs`

```bash
mkdir -p ~/tabs
cdp-fs ~/tabs

# Optional flags:
# -e, --endpoint <URL>   Specify custom CDP endpoint (default: http://localhost:9222)
```

### 3. Explore & Interact with Standard POSIX Tools

```bash
# List all tabs
ls -la ~/tabs

# Check URL and title of a tab
cat "~/tabs/Example Domain/url"
cat "~/tabs/Example Domain/title"

# View plain text content
cat "~/tabs/Example Domain/text"

# View page HTML
cat "~/tabs/Example Domain/html"

# Capture a live screenshot
cat "~/tabs/Example Domain/screenshot.png" > /tmp/screen.png

# Evaluate JavaScript in the tab
echo "document.body.style.backgroundColor = 'yellow'" > "~/tabs/Example Domain/eval"
echo "2 + 2" > "~/tabs/Example Domain/eval"
cat "~/tabs/Example Domain/eval" # Outputs: 4

# Navigate the tab to a new URL
echo "https://news.ycombinator.com" > "~/tabs/Example Domain/url"

# Change the tab title
echo "Custom Title" > "~/tabs/Example Domain/title"

# Reload the tab
touch "~/tabs/Example Domain/reload"

# Open a new tab
mkdir "~/tabs/https://rust-lang.org"

# Close a tab
rm -rf "~/tabs/Example Domain"
```

### 4. Unmount

```bash
fusermount -u ~/tabs
```

---

## Running the Automated Test Suite

`cdp-fs` includes automated unit tests and an end-to-end integration test suite:

```bash
# Run unit tests
cargo test

# Run full integration test with headless Chromium
tests/integration_test.sh
```
