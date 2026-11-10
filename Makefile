SWITCHER    := cosmic-ext-app-switcher
# Optional: maximum number of previews shown at once.
PREVIEW_COUNT ?=
APPLET      := cosmic-ext-applet-app-switcher
INSTALL_DIR := $(HOME)/.local/bin
APPS_DIR    := $(HOME)/.local/share/applications
SWITCHER_BIN := target/release/$(SWITCHER)
APPLET_BIN  := target/release/$(APPLET)
DESKTOP     := applet/data/io.github.cosmic-ext-applet-app-switcher.desktop
ICON_SRC    := applet/data/io.github.cosmic-ext-applet-app-switcher-symbolic.svg
ICONS_DIR   := $(HOME)/.local/share/icons/hicolor/scalable/apps
AUTOSTART_DIR := $(HOME)/.config/autostart
AUTOSTART   := $(AUTOSTART_DIR)/cosmic-ext-app-switcher.desktop

.PHONY: all build install uninstall enable disable status reinstall check-compat quit lint

all: build

build:
	@command -v cargo >/dev/null 2>&1 || { \
		echo "Rust not found. Install with:"; \
		echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"; \
		exit 1; \
	}
	@command -v pkg-config >/dev/null 2>&1 || { \
		echo "pkg-config not found. Install with:"; \
		echo "  sudo apt install pkg-config libxkbcommon-dev libgbm-dev"; \
		exit 1; \
	}
	@pkg-config --exists xkbcommon || { \
		echo "libxkbcommon development files not found. Install with:"; \
		echo "  sudo apt install libxkbcommon-dev"; \
		exit 1; \
	}
	@pkg-config --exists gbm || { \
		echo "libgbm development files not found. Install with:"; \
		echo "  sudo apt install libgbm-dev"; \
		exit 1; \
	}
	@case "$$(command -v cargo)" in \
		/nix/*) \
			echo "Warning: using Nix's cargo. The resulting binary may not find"; \
			echo "the system libwayland-client.so.0 at runtime. For an installed"; \
			echo "shortcut, build with rustup/system Rust instead."; \
			;; \
	esac
	cargo build --release --workspace

install: build
	@if [ -x "$(INSTALL_DIR)/$(SWITCHER)" ]; then \
		"$(INSTALL_DIR)/$(SWITCHER)" --quit >/dev/null 2>&1 || true; \
	fi
	@mkdir -p $(INSTALL_DIR) $(APPS_DIR) $(ICONS_DIR)
	install -Dm755 $(SWITCHER_BIN) $(INSTALL_DIR)/$(SWITCHER)-bin
	install -Dm755 scripts/wrapper.sh $(INSTALL_DIR)/$(SWITCHER)
	install -Dm755 $(APPLET_BIN) $(INSTALL_DIR)/$(APPLET)-bin
	install -Dm755 scripts/wrapper.sh $(INSTALL_DIR)/$(APPLET)
	install -Dm644 $(DESKTOP) $(APPS_DIR)/io.github.cosmic-ext-applet-app-switcher.desktop
	install -Dm644 $(ICON_SRC) $(ICONS_DIR)/io.github.cosmic-ext-applet-app-switcher-symbolic.svg
	@mkdir -p $(AUTOSTART_DIR)
	@printf '%s\n' \
		'[Desktop Entry]' \
		'Type=Application' \
		'Name=COSMIC Ext App Switcher' \
		'Comment=Resident window switcher with thumbnail previews' \
		'Exec=$(INSTALL_DIR)/$(SWITCHER) --daemon$(if $(PREVIEW_COUNT), --preview-count $(PREVIEW_COUNT))' \
		'Terminal=false' \
		'NoDisplay=true' \
		'X-GNOME-Autostart-enabled=true' \
		'OnlyShowIn=COSMIC;' \
		> $(AUTOSTART)
	@$(MAKE) enable
	@echo ""
	@echo "Installed, enabled, and autostarted. Press Super+Tab or Alt+Tab to try it."
	@echo "Add the 'App Switcher Settings' applet to your COSMIC panel to change themes."

uninstall:
	@$(MAKE) disable
	@if [ -x "$(INSTALL_DIR)/$(SWITCHER)" ]; then \
		"$(INSTALL_DIR)/$(SWITCHER)" --quit >/dev/null 2>&1 || true; \
	fi
	@rm -f $(INSTALL_DIR)/$(SWITCHER)
	@rm -f $(INSTALL_DIR)/$(SWITCHER)-bin
	@rm -f $(INSTALL_DIR)/$(APPLET)
	@rm -f $(INSTALL_DIR)/$(APPLET)-bin
	@rm -f $(APPS_DIR)/io.github.cosmic-ext-applet-app-switcher.desktop
	@rm -f $(ICONS_DIR)/io.github.cosmic-ext-applet-app-switcher-symbolic.svg
	@rm -f $(AUTOSTART)
	@echo "Uninstalled. COSMIC default switcher restored."

quit:
	@if [ -x "$(INSTALL_DIR)/$(SWITCHER)" ]; then \
		"$(INSTALL_DIR)/$(SWITCHER)" --quit; \
	else \
		echo "Switcher is not installed."; \
	fi

enable:
	@bash scripts/enable.sh

disable:
	@bash scripts/disable.sh

status:
	@bash scripts/status.sh

reinstall: uninstall install

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

check-compat:
	@echo "Checking compatibility..."
	@bash scripts/find-config.sh > /dev/null 2>&1; case $$? in \
		0) echo "  COSMIC shortcuts config: found" ;; \
		2) echo "  COSMIC shortcuts config: not created yet (run 'make enable')" ;; \
		*) echo "  COSMIC shortcuts config: NOT found" ;; \
	esac
	@test -f $(INSTALL_DIR)/$(SWITCHER)-bin && echo "  Switcher binary: installed" || echo "  Switcher binary: not installed"
	@test -f $(INSTALL_DIR)/$(APPLET)-bin   && echo "  Applet binary:   installed" || echo "  Applet binary:   not installed"
	@command -v cosmic-comp >/dev/null 2>&1 && echo "  cosmic-comp: found" || echo "  cosmic-comp: not found (is COSMIC running?)"
	@bin=$(INSTALL_DIR)/$(SWITCHER); [ -x "$$bin" ] || bin=$(SWITCHER_BIN); \
	if [ ! -x "$$bin" ]; then \
		echo "  Wayland interfaces: not checked (build or install the switcher first)"; \
	else \
		out=$$("$$bin" --check-compat 2>&1); rc=$$?; \
		if [ $$rc -eq 2 ]; then \
			echo "  Wayland interfaces: not checked ($$bin predates --check-compat, rebuild it)"; \
		else \
			echo "  Wayland interfaces:"; \
			echo "$$out" | sed 's/^/    /'; \
		fi; \
	fi
	@echo "Done."
