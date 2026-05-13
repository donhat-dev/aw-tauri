ifeq ($(OS),Windows_NT)
	HOST_OS := Windows
	ARCH :=
else
	SHELL := bash
	ifeq ($(shell uname -m), arm64)
		ARCH := _arm64
	else
		ARCH :=
	endif
	HOST_OS := $(shell uname -s)
endif

WATCHER_DIST ?= ..
-include ../tauri-watchers.mk
TAURI_WATCHERS ?= aw-watcher-input aw-watcher-screenshot-mini aw-odoo-sync

.PHONY: build bundle-watchers stage-runtime-watchers

build: prebuild bundle-watchers
	npm run tauri build
	$(MAKE) stage-runtime-watchers

bundle-watchers:
ifeq ($(OS),Windows_NT)
	@powershell -NoProfile -ExecutionPolicy Bypass -Command "$$watchers = '$(TAURI_WATCHERS)'.Split(' ', [System.StringSplitOptions]::RemoveEmptyEntries); New-Item -ItemType Directory -Force 'resources' | Out-Null; foreach ($$watcher in $$watchers) { $$srcDir = Join-Path '$(WATCHER_DIST)' (Join-Path $$watcher (Join-Path 'dist' $$watcher)); $$srcExe = Join-Path '$(WATCHER_DIST)' (Join-Path $$watcher ('dist/' + $$watcher + '.exe')); $$target = Join-Path 'resources' $$watcher; if (Test-Path -LiteralPath $$target) { Remove-Item -LiteralPath $$target -Recurse -Force }; if (Test-Path -LiteralPath $$srcExe -PathType Leaf) { New-Item -ItemType Directory -Force $$target | Out-Null; Copy-Item -LiteralPath $$srcExe -Destination (Join-Path $$target ($$watcher + '.exe')) -Force; Write-Host \"Bundled $$watcher from $$srcExe\" } elseif (Test-Path -LiteralPath $$srcDir -PathType Container) { New-Item -ItemType Directory -Force $$target | Out-Null; Copy-Item -Path (Join-Path $$srcDir '*') -Destination $$target -Recurse -Force; Write-Host \"Bundled $$watcher from $$srcDir\" } else { Write-Host \"Skipping $$watcher (not found at $$srcExe or $$srcDir)\" } }"
else
	@mkdir -p resources
	@for watcher in $(TAURI_WATCHERS); do \
		src_dir="$(WATCHER_DIST)/$$watcher/dist/$$watcher"; \
		src_exe="$(WATCHER_DIST)/$$watcher/dist/$$watcher.exe"; \
		target="resources/$$watcher"; \
		rm -rf "$$target"; \
		if [ -d "$$src_dir" ]; then \
			mkdir -p "$$target"; \
			cp -r "$$src_dir/." "$$target/"; \
			echo "Bundled $$watcher from $$src_dir"; \
		elif [ -f "$$src_exe" ]; then \
			mkdir -p "$$target"; \
			cp "$$src_exe" "$$target/$$watcher.exe"; \
			echo "Bundled $$watcher from $$src_exe"; \
		else \
			echo "Skipping $$watcher (not found at $$src_dir or $$src_exe)"; \
		fi; \
	done
endif

stage-runtime-watchers: bundle-watchers
ifeq ($(OS),Windows_NT)
	@powershell -NoProfile -ExecutionPolicy Bypass -Command "if (Test-Path -LiteralPath 'src-tauri/target/release' -PathType Container) { $$watchers = '$(TAURI_WATCHERS)'.Split(' ', [System.StringSplitOptions]::RemoveEmptyEntries); foreach ($$watcher in $$watchers) { $$src = Join-Path 'resources' $$watcher; $$target = Join-Path 'src-tauri/target/release' $$watcher; if (Test-Path -LiteralPath $$src -PathType Container) { if (Test-Path -LiteralPath $$target) { Remove-Item -LiteralPath $$target -Recurse -Force }; New-Item -ItemType Directory -Force $$target | Out-Null; Copy-Item -Path (Join-Path $$src '*') -Destination $$target -Recurse -Force; Write-Host \"Staged $$watcher into $$target\" } else { Write-Host \"Skipping runtime stage for $$watcher (not found at $$src)\" } } } else { Write-Host 'Skipping runtime watcher staging (src-tauri/target/release does not exist)' }"
else
	@if [ -d "src-tauri/target/release" ]; then \
		for watcher in $(TAURI_WATCHERS); do \
			src="resources/$$watcher"; \
			target="src-tauri/target/release/$$watcher"; \
			if [ -d "$$src" ]; then \
				rm -rf "$$target"; \
				mkdir -p "$$target"; \
				cp -r "$$src/." "$$target/"; \
				echo "Staged $$watcher into $$target"; \
			else \
				echo "Skipping runtime stage for $$watcher (not found at $$src)"; \
			fi; \
		done; \
	else \
		echo "Skipping runtime watcher staging (src-tauri/target/release does not exist)"; \
	fi
endif

dev: prebuild
	npm run tauri dev

%/.git:
	git submodule update --init --recursive

src-tauri/icons/icon.png: aw-webui/.git
	mkdir -p src-tauri/icons
	npm run tauri icon "./aw-webui/media/logo/logo.png"

aw-webui/dist: aw-webui/.git
	$(MAKE) -C aw-webui build SHELL=bash

prebuild: aw-webui/dist node_modules src-tauri/icons/icon.png

precommit: format check

format:
	cd src-tauri && cargo fmt

check:
	cd src-tauri && cargo check && cargo clippy

package:
ifeq ($(HOST_OS),Linux)
	rm -rf target/package/aw-tauri
	mkdir -p target/package/aw-tauri
	cp src-tauri/target/release/bundle/deb/*.deb target/package/aw-tauri/aw-tauri$(ARCH).deb
	cp src-tauri/target/release/bundle/rpm/*.rpm target/package/aw-tauri/aw-tauri$(ARCH).rpm
	cp src-tauri/target/release/bundle/appimage/*.AppImage target/package/aw-tauri/aw-tauri$(ARCH).AppImage

	mkdir -p dist/aw-tauri
	rm -rf dist/aw-tauri/*
	cp target/package/aw-tauri/* dist/aw-tauri/
else
	rm -rf target/package
	mkdir -p target/package
	cp src-tauri/target/release/aw-tauri target/package/aw-tauri

	mkdir -p dist
	find dist/ -maxdepth 1 -type f -delete 2>/dev/null || true
	cp target/package/* dist/
endif

node_modules: package-lock.json
	npm ci
