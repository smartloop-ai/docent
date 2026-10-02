#!/bin/sh
# Smartloop CLI installer.
#
#   curl -fsSL https://smartloop.ai/install | sh
#
# Environment:
#   SMARTLOOP_CLI_VERSION      Version to install (default: latest release)
#   SMARTLOOP_CLI_INSTALL_DIR  Install directory (default: $CARGO_HOME/bin when a
#                              Rust toolchain is present, else ~/.local/bin)
set -eu

REPO="smartloop-ai/smartloop-cli"

# Colors, only when stdout is a terminal.
if [ -t 1 ]; then
    MUTED='\033[0;2m'
    PINK='\033[38;5;205m'
    GREEN='\033[1;32m'
    RED='\033[0;31m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    MUTED=''; PINK=''; GREEN=''; RED=''; BOLD=''; NC=''
fi

# dash's echo has no -e and prints it literally, so every message goes through
# printf %b -- %b expands the backslash escapes the colour variables hold,
# which a printf format string would expand but an argument would not.
say() { printf '%b\n' "$1"; }

error() { say "${RED}Error:${NC} $1" >&2; exit 1; }

# One trap for both jobs: the scratch directory, and the cursor that
# download_with_progress hides -- a crash mid-download would otherwise leave it
# hidden in the user's terminal.
cleanup() {
    # Only when stdout is a terminal, or the escape shows up as literal bytes
    # in redirected output.
    [ -t 1 ] && printf '\033[?25h' 2>/dev/null
    [ -n "${TMP_DIR:-}" ] && rm -rf "$TMP_DIR"
    return 0
}
trap cleanup EXIT INT TERM

need() {
    command -v "$1" >/dev/null 2>&1 || error "$1 is required but not installed"
}

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Map uname output onto the Rust target triple the release is built for.
detect_target() {
    local os arch os_part arch_part
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os_part="unknown-linux-musl" ;;
        Darwin) os_part="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os_part="pc-windows-msvc" ;;
        *) error "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch_part="x86_64" ;;
        arm64|aarch64) arch_part="aarch64" ;;
        *) error "Unsupported architecture: $arch" ;;
    esac

    if [ "$os_part" = "pc-windows-msvc" ] && [ "$arch_part" != "x86_64" ]; then
        error "Windows builds are published for x86_64 only"
    fi

    OS="$os_part"
    TARGET="${arch_part}-${os_part}"
    if [ "$os_part" = "pc-windows-msvc" ]; then
        ARCHIVE_EXT="zip"
        BIN_NAME="smartloop.exe"
    else
        ARCHIVE_EXT="tar.gz"
        BIN_NAME="smartloop"
    fi
}

# Latest release tag, read straight off the redirect so no JSON parser and no
# API rate limit are involved.  A repo with no releases redirects to
# /releases instead of /releases/tag/vX.Y.Z, so require the /tag/ segment
# rather than handing the caller back a URL as if it were a version.
latest_version() {
    local url
    url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest")" || return 1

    case "$url" in
        */tag/v*) printf '%s\n' "${url##*/tag/v}" ;;
        *) return 1 ;;
    esac
}

verify_checksum() {
    local archive="$1" sums="$2" actual expected

    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$archive" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
    else
        say "${MUTED}No sha256 tool found; skipping checksum verification${NC}"
        return 0
    fi

    expected="$(grep " $(basename "$archive")\$" "$sums" | awk '{print $1}')"
    [ -n "$expected" ] || error "No checksum published for $(basename "$archive")"
    [ "$actual" = "$expected" ] || error "Checksum mismatch for $(basename "$archive")"
}

# Integer arithmetic only -- the reference installer shells out to bc for this,
# but bc is absent from many minimal images and one decimal place does not
# warrant the dependency.
format_bytes() {
    local n="$1"
    if [ "$n" -ge 1073741824 ]; then
        printf '%d.%d GB' "$(( n / 1073741824 ))" "$(( (n % 1073741824) * 10 / 1073741824 ))"
    elif [ "$n" -ge 1048576 ]; then
        printf '%d.%d MB' "$(( n / 1048576 ))" "$(( (n % 1048576) * 10 / 1048576 ))"
    else
        printf '%d KB' "$(( n / 1024 ))"
    fi
}

# progress_bar -- adapted from progress-bar.sh by Edouard Lopez
# https://github.com/edouard-lopez/progress-bar.sh -- MIT License
progress_bar() {
    local bytes="$1" length="$2" label="${3:-Downloading}"
    [ "$length" -gt 0 ] || return 0

    local columns space_reserved=6
    columns="$(tput cols 2>/dev/null || echo 80)"
    local space_available=$(( columns - space_reserved ))
    [ "$space_available" -lt 10 ] && space_available=10

    local percent=$(( bytes * 100 / length ))
    [ "$percent" -gt 100 ] && percent=100

    local filled=$(( percent * space_available / 100 ))

    local bar="" i=0
    while [ "$i" -lt "$filled" ]; do bar="${bar}▇"; i=$((i + 1)); done
    while [ "$i" -lt "$space_available" ]; do bar="${bar} "; i=$((i + 1)); done

    local dl_str total_str
    dl_str="$(format_bytes "$bytes")"
    total_str="$(format_bytes "$length")"

    # Two-line display: label on line 1, bar on line 2
    printf "\r\033[K${MUTED}%s  %s / %s${NC}\n\r\033[K%s| %3d%%\033[1A\r" \
        "$label" "$dl_str" "$total_str" "$bar" "$percent"
}

end_progress() {
    # Move the cursor past the two-line progress display and restore it
    printf "\n\n\033[?25h"
}

download_with_progress() {
    local url="$1" output="$2" label="${3:-Downloading}"
    local length=0 bytes=0

    length="$(curl -sI -L "$url" | grep -i content-length | tail -1 | awk '{print $2}' | tr -d '\r')"
    length="${length:-0}"

    if [ "$length" -gt 0 ] && [ -t 2 ]; then
        printf "\033[?25l\n"
        curl -sL "$url" -o "$output" &
        local curl_pid=$!

        while kill -0 "$curl_pid" 2>/dev/null; do
            if [ -f "$output" ]; then
                bytes="$(wc -c < "$output" 2>/dev/null | tr -d ' ')" || true
                bytes="${bytes:-0}"
                progress_bar "$bytes" "$length" "$label"
            fi
            sleep 0.1
        done

        local ret=0
        wait "$curl_pid" || ret=$?
        progress_bar "$length" "$length" "$label"
        end_progress
        [ "$ret" -eq 0 ] || error "Download failed: $url"
    else
        curl -fL --progress-bar "$url" -o "$output" || error "Download failed: $url"
    fi
}

# Where the binary lands.  A Rust toolchain already has $CARGO_HOME/bin on
# PATH, so installing there leaves nothing to configure.  Otherwise use
# ~/.local/bin, the conventional home for user-installed binaries, and put it
# on PATH below if it is not there already.
default_install_dir() {
    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    if [ -d "$cargo_bin" ] && [ -w "$cargo_bin" ]; then
        printf '%s\n' "$cargo_bin"
    else
        printf '%s\n' "$HOME/.local/bin"
    fi
}

add_to_path() {
    local config_file="$1" command="$2"

    # Idempotent: re-running the installer must not stack duplicate exports.
    if grep -Fxq "$command" "$config_file" 2>/dev/null; then
        return 0
    fi

    if [ -w "$config_file" ]; then
        printf '\n# smartloop\n%s\n' "$command" >> "$config_file"
        say "${MUTED}Added ${NC}smartloop${MUTED} to \$PATH in ${NC}${config_file}"
    else
        say "${MUTED}Manually add to ${NC}${config_file}${MUTED}:${NC}"
        say "  $command"
    fi
}

# Nothing this script does can change the PATH of the shell that invoked it --
# piped into bash, it is a child process -- so persist the change in the rc
# file the user's shell reads at startup instead.
setup_path() {
    local dir="$1" current_shell config_files path_command config_file="" f

    on_path "$dir" && return 0

    current_shell="$(basename "${SHELL:-bash}")"

    case "$current_shell" in
        fish)
            config_files="$HOME/.config/fish/config.fish"
            path_command="fish_add_path $dir"
            ;;
        zsh)
            config_files="${ZDOTDIR:-$HOME}/.zshrc"
            path_command="export PATH=\"${dir}:\$PATH\""
            ;;
        bash)
            config_files="$HOME/.bashrc $HOME/.bash_profile $HOME/.profile"
            path_command="export PATH=\"${dir}:\$PATH\""
            ;;
        *)
            config_files="$HOME/.bashrc $HOME/.profile"
            path_command="export PATH=\"${dir}:\$PATH\""
            ;;
    esac

    for f in $config_files; do
        if [ -f "$f" ]; then
            config_file="$f"
            break
        fi
    done

    if [ -z "$config_file" ]; then
        say "${MUTED}No config file found for ${NC}${current_shell}${MUTED}. Manually add to your shell config:${NC}"
        say "  $path_command"
        return 0
    fi

    add_to_path "$config_file" "$path_command"
}

print_banner() {
    say ""
    say "${PINK}█▀▄ █▀█ █▀▀ █▀▀ █▄ █ ▀█▀${NC}"
    say "${PINK}█▄▀ █▄█ █▄▄ ██▄ █ ▀█  █${NC}  ${MUTED}by Smartloop${NC}"
    say "${MUTED}Your private AI assistant${NC}"
    say ""
    say "${MUTED}Version: ${NC}${VERSION}"
    say ""

    if on_path "$INSTALL_DIR"; then
        say "${MUTED}To get started:${NC}"
        say ""
        say "  docent                  ${MUTED}# Start chatting${NC}"
    else
        say "${MUTED}To get started, restart your terminal or run:${NC}"
        say ""
        case "$(basename "${SHELL:-bash}")" in
            fish) say "  source ~/.config/fish/config.fish" ;;
            zsh)  say "  source ${ZDOTDIR:-$HOME}/.zshrc" ;;
            *)    say "  source ~/.bashrc" ;;
        esac
        say ""
        say "${MUTED}Then run:${NC}"
        say ""
        say "  docent                  ${MUTED}# Start chatting${NC}"
    fi

    say ""
    say "${MUTED}For more information visit ${NC}https://smartloop.ai/docs/intro/"
    say ""
}

install_smartloop() {
    need curl
    need tar

    say "${MUTED}Reading package lists...${NC}"
    detect_target
    say "${MUTED}Reading package lists... Done${NC}"

    VERSION="${SMARTLOOP_CLI_VERSION:-$(latest_version || true)}"
    [ -n "$VERSION" ] || error "Could not determine the latest release; set SMARTLOOP_CLI_VERSION"
    VERSION="${VERSION#v}"

    INSTALL_DIR="${SMARTLOOP_CLI_INSTALL_DIR:-$(default_install_dir)}"

    local name="smartloop-${VERSION}-${TARGET}"
    local archive="${name}.${ARCHIVE_EXT}"
    local base_url="https://github.com/${REPO}/releases/download/v${VERSION}"

    say "${MUTED}The following NEW packages will be installed:${NC}"
    say "  ${BOLD}smartloop${NC} ${MUTED}(${VERSION}, ${TARGET})${NC}"

    TMP_DIR="$(mktemp -d)"

    say "${MUTED}[1/3] Downloading smartloop (${VERSION})${NC}"
    download_with_progress "${base_url}/${archive}" "${TMP_DIR}/${archive}" \
        "Get:1 smartloop ${VERSION}"
    curl -fsSL "${base_url}/SHA256SUMS" -o "${TMP_DIR}/SHA256SUMS" \
        || error "Could not download SHA256SUMS"
    verify_checksum "${TMP_DIR}/${archive}" "${TMP_DIR}/SHA256SUMS"

    say "${MUTED}[2/3] Unpacking smartloop (${VERSION})${NC}"
    if [ "$ARCHIVE_EXT" = "zip" ]; then
        need unzip
        unzip -q "${TMP_DIR}/${archive}" -d "$TMP_DIR"
    else
        tar -xzf "${TMP_DIR}/${archive}" -C "$TMP_DIR"
    fi

    say "${MUTED}[3/3] Setting up smartloop (${VERSION})${NC}"
    mkdir -p "$INSTALL_DIR"
    install -m 755 "${TMP_DIR}/${name}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}" 2>/dev/null \
        || { cp "${TMP_DIR}/${name}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}" \
             && chmod 755 "${INSTALL_DIR}/${BIN_NAME}"; }

    "${INSTALL_DIR}/${BIN_NAME}" --version >/dev/null 2>&1 \
        || error "Installation verification failed: 'smartloop --version' did not succeed"

    # `docent` is the same binary under its other name: a symlink, or on
    # Windows (no symlinks without admin) a copy.
    if [ "$OS" = "pc-windows-msvc" ]; then
        cp "${INSTALL_DIR}/${BIN_NAME}" "${INSTALL_DIR}/docent.exe"
    else
        ln -sf "$BIN_NAME" "${INSTALL_DIR}/docent"
    fi

    say "${GREEN}Installed${NC} ${INSTALL_DIR}/${BIN_NAME} ${MUTED}(also as${NC} docent${MUTED})${NC}"

    say "${MUTED}Processing triggers for smartloop (${VERSION}) ...${NC}"
    if [ "$OS" = "pc-windows-msvc" ]; then
        # No POSIX rc file to edit under MSYS/Cygwin; install.ps1 sets the real
        # Windows user PATH.
        on_path "$INSTALL_DIR" || {
            say "${MUTED}Add the following to your PATH:${NC}"
            say "  ${BOLD}${INSTALL_DIR}${NC}"
        }
    else
        setup_path "$INSTALL_DIR"
    fi

    print_banner
}

install_smartloop
