# space-mesh — Rust 코어 + SwiftUI 앱 빌드/테스트
#
# 의존 흐름: core(release) → uniffi 바인딩 생성 → app(swift) 링크
# app은 core/target/release/libspace_ffi.dylib를 링크하므로 core release 빌드가 선행되어야 한다.

CORE        := core
APP         := app
VERSION     := $(shell sed -n 's/^version = "\([^"]*\)"/\1/p' $(CORE)/Cargo.toml | head -1)
CORE_MANIFEST := $(CORE)/Cargo.toml
DYLIB       := $(CORE)/target/release/libspace_ffi.dylib
BINDINGS_OUT := $(APP)/Sources
GEN_TMP     := $(CORE)/target/uniffi-gen
CARGO       := cargo
CARGO_FLAGS := --release

.DEFAULT_GOAL := help

# ─────────────────────────── 메타 ───────────────────────────

.PHONY: help
help: ## 사용 가능한 타깃 목록
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: all
all: core bindings app ## 전체 빌드 (core → 바인딩 → app)

# ─────────────────────────── Rust 코어 ───────────────────────────

.PHONY: core
core: ## Rust 워크스페이스 release 빌드 (FFI dylib 포함)
	$(CARGO) build $(CARGO_FLAGS) --manifest-path $(CORE_MANIFEST)

.PHONY: core-test
core-test: ## Rust 전체 테스트
	$(CARGO) test $(CARGO_FLAGS) --manifest-path $(CORE_MANIFEST)

.PHONY: cli
cli: ## space-mesh CLI 바이너리 빌드
	$(CARGO) build $(CARGO_FLAGS) --manifest-path $(CORE_MANIFEST) --bin space-mesh

# ─────────────────────────── UniFFI 바인딩 ───────────────────────────

.PHONY: bindings
bindings: core ## dylib에서 Swift 바인딩 생성 후 app 소스에 배치
	# uniffi-bindgen이 cwd 기준으로 cargo metadata를 부르므로 core 안에서 실행한다.
	cd $(CORE) && $(CARGO) run $(CARGO_FLAGS) --features cli --bin uniffi-bindgen -q -- \
		generate --library target/release/libspace_ffi.dylib --language swift --out-dir target/uniffi-gen
	cp $(GEN_TMP)/space_ffiFFI.h        $(BINDINGS_OUT)/space_ffiFFI/include/
	cp $(GEN_TMP)/space_ffiFFI.modulemap $(BINDINGS_OUT)/space_ffiFFI/include/module.modulemap
	cp $(GEN_TMP)/space_ffi.swift        $(BINDINGS_OUT)/SpaceMeshCore/
	@echo "✓ Swift 바인딩 갱신됨"

# ─────────────────────────── Swift 앱 ───────────────────────────

.PHONY: app
app: bindings ## SwiftPM 앱 빌드 (core+바인딩 선행)
	cd $(APP) && swift build

.PHONY: app-release
app-release: bindings ## SwiftPM 앱 release 빌드
	cd $(APP) && swift build -c release

.PHONY: package
package: ## Homebrew/GitHub Release용 .app zip 생성
	VERSION="$(VERSION)" ./scripts/package-app.sh

.PHONY: run
run: app ## 앱 빌드 후 실행
	$(APP)/.build/debug/SpaceMeshApp

# ─────────────────────────── 테스트 ───────────────────────────

.PHONY: selftest
selftest: app ## 앱 FFI 경로 headless 검증 (--selftest)
	cd $(APP) && swift run -q SpaceMeshApp --selftest

.PHONY: test
test: core-test selftest ## 전체 테스트 (Rust + Swift selftest)
	@echo "✓ 모든 테스트 통과"

# ─────────────────────────── 품질 ───────────────────────────

.PHONY: fmt
fmt: ## Rust 포맷 (cargo fmt은 --manifest-path 미지원 → cd 필요)
	cd $(CORE) && $(CARGO) fmt

.PHONY: fmt-check
fmt-check: ## 포맷 diff만 확인 (CI용, 미적용)
	cd $(CORE) && $(CARGO) fmt --check

.PHONY: clippy
clippy: ## Rust 린트 (전체 타깃, 경고=에러)
	cd $(CORE) && $(CARGO) clippy $(CARGO_FLAGS) --all-targets -- -D warnings

.PHONY: check
check: fmt clippy core-test ## 포맷 + 린트 + 테스트

# ─────────────────────────── 정리 ───────────────────────────

.PHONY: clean
clean: ## 빌드 산출물 삭제 (core target + app .build)
	$(CARGO) clean --manifest-path $(CORE_MANIFEST)
	rm -rf $(APP)/.build
	rm -rf dist

.PHONY: bench-git
bench-git: core ## git repo 조회 실측 (인자: ROOT=경로)
	@ROOT="$${ROOT:-$$HOME/work/project}"; \
	echo "git probe 벤치: $$ROOT"; \
	$(CARGO) run --manifest-path $(CORE_MANIFEST) $(CARGO_FLAGS) -q --bin space-mesh -- "$$ROOT" --depth 0 >/dev/null
