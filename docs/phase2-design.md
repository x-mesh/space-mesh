# space-mesh Phase 2 기능 설계

> 상태: M4 구현됨 (F1·F2·F9 — 회수 루프), M5 구현됨 (F3·F4·F5·F7 — 정확도), M6 설계 단계 · 기준 코드: `main` @ e4c3242 · 작성일: 2026-07-07

## 1. 배경 — Phase 1이 이미 하는 것

Phase 1(M1~M3, t2)은 "어디가 큰지 확인"까지의 파이프라인을 완성했다.

| 영역 | 구현 | 위치 |
|---|---|---|
| 병렬 스캔 | rayon 순회, logical/allocated 동시 집계, 하드링크 1회 계산, 진행 폴링 | `core/scanner` |
| 스냅샷/diff | SQLite 저장, 시계열 목록, 잔차 귀속 diff + drilldown(`DiffHandle`) | `core/index` |
| 불필요 파일 | 고정 경로 룰(`rules.json`) + 트리 내 카테고리 탐지(마커 검증, idle 일수) + 도구 advisor(dry-run 예상치) | `core/rules` |
| 중복 탐지 | 크기 → 부분 해시 → blake3 3단 필터 | `core/dedup` |
| git 건강도 | 위험도 분류, ahead/dirty/stash, 캐시(TTL+signature), 주별 활동 | `core/git` |
| 앱 | 트리맵 drilldown, 변화/빌드 산출물/Git/정리/중복 탭, 휴지통+undo, 안전 가드 | `app/` |
| 백그라운드 | launchd 주기 스냅샷, FSEvents 실시간 감시, 예산 알림 | `BackgroundAgent` |

사용자 여정 "확인 → 검증 → 회수" 중 **확인·검증은 강하고, 회수는 탭마다 파편화**되어 있으며, 회수 이후의 확인(실제로 얼마나 비워졌는가)은 없다.

## 2. Phase 2 목표와 비목표

### 목표 (테마 3개)

- **A. 회수 루프 완성** — 흩어진 후보를 하나의 플랜으로 모아 실행하고, *실제 회수량*을 검증해 보여준다. 신뢰(브랜드 원칙 1순위)를 회수 단계까지 확장.
- **B. 정확도와 신선도** — APFS 클론/로컬 스냅샷 때문에 생기는 "지웠는데 왜 안 늘지"를 설명하고, 전체 재스캔 없이 트리를 신선하게 유지한다.
- **C. 자동화의 반 걸음** — 사용자가 열지 않아도 에이전트가 후보를 계산해 *제안*까지만 한다. 자동 삭제는 하지 않는다 (비목표 참조).

### 비목표

- 사용자 확인 없는 자동 삭제. 어떤 모드에서도 파괴적 행위는 명시적 클릭을 거친다.
- 홈 디렉토리 밖 시스템 영역 정리 (SIP/권한 지뢰밭 — Phase 3 이후).
- iCloud/클라우드 동기화 상태 인식 (별도 조사 필요, 부록 참조).
- 크로스 플랫폼 (macOS 전용 유지).

## 3. 기능 요약과 우선순위

| ID | 기능 | 테마 | 우선순위 | 크기 |
|---|---|---|---|---|
| F1 | 통합 회수 플랜 + 실행 후 검증 | A | **P0** | L |
| F2 | 증분 재스캔 (서브트리 리프레시) | B | **P0** | M |
| F3 | APFS 클론 인식 + 클론 병합 dedup | B | **P1** | M |
| F4 | Purgeable/로컬 스냅샷 인식 | B | **P1** | S |
| F5 | mtime 수집 + 나이 히트맵/필터 | B·C | **P1** | M |
| F6 | 정책 기반 백그라운드 제안 | C | **P1** | M |
| F7 | 스냅샷 보존 정책 (DB 다이어트) | B | **P2** | S |
| F8 | CLI 확장 (detect/dups/advise + JSON) | C | **P2** | S |
| F9 | Full Disk Access 온보딩 | A | **P2** | S |

의존 관계: F1 ← F2 (검증 재스캔), F5 ← 스키마 변경, F6 ← F1(플랜 모델)·F8(CLI 탐지). F3·F4는 독립.

```
M4 (회수 루프):   F2 → F1 → F9
M5 (정확도):      F3, F4, F5, F7
M6 (자동화):      F8 → F6
```

---

## 4. 기능 상세

### F1. 통합 회수 플랜 (Reclaim Plan) — P0

**문제.** 정리 후보가 4곳(빌드 산출물·정리·중복·트리맵 big files)에 흩어져 있고 선택 상태(`selectedCleanupPaths`, `selectedDupPaths`)도 탭별로 분리. 사용자는 "이번에 총 얼마를 회수하는지" 한 번에 볼 수 없고, 실행 후 예상이 맞았는지도 알 수 없다.

**설계.** 모든 탭에서 "플랜에 추가"할 수 있는 장바구니 하나를 도입한다.

- **모델 (`app/ReclaimPlan.swift`, 신규)**
  ```swift
  struct PlanItem: Identifiable {
      let path: String
      let estimatedBytes: UInt64
      let source: PlanSource   // .category(id) | .rule(id) | .duplicate(hash) | .bigFile
      let safety: String       // "safe" | "warn"
      let recreateCommand: String
  }
  @MainActor final class ReclaimPlan: ObservableObject {
      @Published var items: [PlanItem]
      // 조상/자손 중복 방지: CleanupModel.toggleCleanupSelection의 prefix 로직을 이관·공용화
  }
  ```
  기존 `CleanupModel`의 trash/undo/`isSafeToTrash`는 그대로 실행 엔진으로 재사용하고, 선택 상태만 플랜으로 승격한다.

- **UI.** 우측 하단 고정 트레이(계기판의 "적산계" 느낌): `N개 항목 · 예상 회수 12.4 GiB` + `EXECUTE`. 클릭 시 시트에서 항목별 safety/재생성 비용 검토 → 실행. warn 항목은 기본 체크 해제 상태로 노출 (원칙 4: 파괴적 행위일수록 조용하고 명확하게).

- **실행 후 검증 (F2 의존).** 실행 직후 영향받은 서브트리만 증분 재스캔해 `예상 12.4 GiB → 실제 11.9 GiB (측정)` 를 리포트. 차이가 크면 원인 후보(F4의 purgeable/스냅샷)를 함께 표시. 검증 결과는 스냅샷 DB에 `reclaim_log` 테이블로 남겨 "변화" 탭에서 마커로 표시한다.
  ```sql
  CREATE TABLE reclaim_log (
    id INTEGER PRIMARY KEY, executed_at TEXT,
    item_count INTEGER, estimated INTEGER, measured INTEGER, undone INTEGER DEFAULT 0
  );
  ```

- **FFI 변경.** 없음 (실행은 Swift `trashItem`, 검증은 F2의 `refresh_paths` 재사용).

**수용 기준.** 세 탭에서 담은 항목이 한 트레이에 합산되고, 실행 → undo → 재실행이 안전 가드를 유지하며, 실행 리포트에 측정 회수량이 표시된다. selftest에 플랜 병합(조상/자손 규칙) 케이스 추가.

### F2. 증분 재스캔 — P0

**문제.** live 모드의 `recompute()`와 F1의 검증이 모두 **전체 재스캔**이다. 홈 전체 기준 수십 초·IO 부담이 커서, FSEvents가 이미 알려주는 "어디가 변했는지"를 버리고 있다.

**설계.** 변경된 경로의 서브트리만 다시 스캔해 메모리 트리에 접합(splice)하고 조상 집계값을 갱신한다.

- **core/scanner** — 신규 API:
  ```rust
  /// root_node 안의 rel_path 서브트리를 다시 스캔해 교체하고,
  /// 조상 체인의 logical/allocated/file_count/dir_count를 델타로 갱신한다.
  pub fn rescan_subtree(root: &mut DirNode, root_path: &Path,
                        rel_path: &Path, opts: &ScanOptions) -> io::Result<SubtreeDelta>;
  ```
  주의: 하드링크 `seen_hardlinks`는 스캔 단위 상태라 서브트리 재스캔에서 전역 일관성이 깨질 수 있다. Phase 2에서는 "같은 서브트리 내 하드링크만 접는다"로 완화하고 문서화한다 (전역 정확도는 다음 전체 스캔 때 회복 — du도 동일한 한계).

- **core/ffi** — `ScanHandle`은 현재 불변(`&self`) 공유 객체. 내부를 `RwLock<DirNode>`로 감싸고:
  ```rust
  pub fn refresh_paths(&self, abs_paths: Vec<String>) -> Result<RefreshSummary, ScanError>;
  // RefreshSummary { changed_paths: u32, delta_allocated: i64, elapsed_ms: u64 }
  ```
  경로 정규화: 스캔 루트 밖 경로는 무시, 서로 포함 관계인 경로는 최상위만 남긴다. 기존 조회 API는 read lock으로 무변경.

- **app/BackgroundAgent** — FSEvents 콜백이 지금은 경로를 버린다(`onFSEvents()` 인자 없음). 이벤트 경로를 수집해 debounce 후 `refresh_paths`로 전달. 전체 재스캔은 (a) 변경 경로가 임계값(예: 512개) 초과, (b) 루트 이벤트 플래그(`kFSEventStreamEventFlagMustScanSubDirs`) 수신 시로 강등.

- **스냅샷 정합성.** 증분 갱신된 트리는 저장 시점에 통째로 `save_snapshot` — 스냅샷 포맷은 변경 없음.

**수용 기준.** 1개 파일 변경 시 재집계가 서브트리 크기에 비례(홈 전체 대비 100배 이상 단축, `cache_bench` 스타일 벤치 추가). 증분 결과 == 전체 스캔 결과 (하드링크 케이스 제외) 를 검증하는 property 테스트.

### F3. APFS 클론 인식 + 클론 병합 dedup — P1

**문제 두 가지.**
1. `st_blocks`는 APFS clonefile 공유 블록을 각 파일에 중복 보고한다 → 중복 그룹의 `reclaimable`이 과대평가되고, 이미 클론인 쌍을 지워도 공간이 안 늘어난다. "신뢰 최우선" 브랜드에 정면으로 어긋나는 지점.
2. 중복을 지우는 것 말고 **더 안전한 회수**가 있다: 두 파일을 clonefile로 병합하면 데이터를 하나도 잃지 않고 공간만 회수된다.

**설계.**

- **클론 감지 (`core/dedup`).** macOS에는 FIEMAP이 없으므로 휴리스틱 계단:
  ① 같은 (dev, ino) → 하드링크(이미 처리됨). ② `fcntl(F_LOG2PHYS)`로 첫 블록 물리 오프셋 비교 — 같으면 클론으로 판정. ③ 판정 불가 시 "회수량 불확실" 플래그.
  `DupGroup`에 `shared_extents: bool` 추가, `reclaimable` 계산에서 클론 쌍은 0으로 집계. FFI `DupGroupInfo`에도 동일 필드 노출, UI는 배지(`이미 클론 — 회수 0`)로 표시.

- **클론 병합 액션 (`core/dedup` 신규).**
  ```rust
  /// keep을 원본으로 남기고 victim을 clonefile 사본으로 교체한다.
  /// 임시파일 + rename으로 원자적 교체, 실패 시 원본 무손상.
  pub fn merge_as_clone(keep: &Path, victim: &Path) -> io::Result<u64>; // 반환: 회수 바이트
  ```
  구현: `clonefile(2)` → 교체 전 재해시로 동일성 재확인(TOCTOU 방지) → `rename`. 메타데이터(mtime, 권한)는 victim 것을 보존. FFI로 `merge_duplicates(keep: String, victims: Vec<String>)` 노출.

- **UI.** 중복 탭의 그룹 액션에 "삭제" 옆 **"클론으로 병합 (무손실)"** 추가. safe 액션이므로 앰버가 아닌 그린 처리. F1 플랜에도 `.duplicate` 항목의 실행 방식으로 선택 가능.

**리스크.** `F_LOG2PHYS` 휴리스틱은 오프셋 우연 일치·파편화에 취약 → 판정은 "회수량 표시"에만 쓰고 삭제 가드에는 쓰지 않는다. 병합은 재해시로 이중 확인하므로 데이터 손실 경로 없음. 외장/비-APFS 볼륨에서는 `clonefile`이 ENOTSUP → 액션 자동 숨김.

### F4. Purgeable 공간·로컬 스냅샷 인식 — P1

**문제.** Time Machine 로컬 스냅샷이 있으면 삭제해도 여유 공간이 즉시 늘지 않는다. F1의 "예상 vs 실제" 차이의 최대 원인.

**설계.** 앱 레벨(Swift)로 충분 — 코어 변경 없음.

- `URL.volumeAvailableCapacityForImportantUsageKey` vs `volumeAvailableCapacityKey` 차이로 purgeable 추정.
- `tmutil listlocalsnapshots /` 파싱으로 스냅샷 개수·날짜 표시 (실행만, 삭제 명령은 advisor 카드로 `tmutil deletelocalsnapshots` *제안*만).
- 표시 위치: 툴바 우측 볼륨 게이지(사용/ purgeable / 여유 3분할 — 계기판 눈금 스타일), F1 실행 리포트의 차이 설명란.

### F5. mtime 수집 + 나이 히트맵/필터 — P1

**문제.** "오래된 것"이 회수 판단의 핵심 신호인데 현재 트리에는 시간 축이 없다 (idle은 git 프로젝트에만 있음).

**설계.**

- **core/scanner.** `FileEntry`에 `mtime: i64`, `DirNode`에 `newest_mtime: i64`(서브트리 최신값) 추가. 스캔 중 이미 `metadata()`를 읽으므로 **추가 IO 없음**.
- **core/index.** `nodes`/`big_files` 테이블에 컬럼 추가. 마이그레이션: `PRAGMA user_version` 도입(현재 0 → 1), 구버전 DB는 열 추가 후 0으로 채움. `load_*`는 결측을 "나이 미상"으로 처리.
- **FFI.** `NodeInfo`/`BigFile`에 필드 추가 (Record 확장은 바인딩 재생성으로 흡수).
- **UI.**
  - 트리맵 오버레이 토글 `크기 | 나이`: 나이 모드에서 타일 밝기를 `newest_mtime` 기준 5단계로 (최근=밝음, 2년+=어두움). 색은 기존 데이터 팔레트 유지, 밝기만 조절 — 원칙 2(색은 의미 있을 때만) 준수.
  - big files 목록에 나이 컬럼 + `90일+ / 1년+` 필터 칩.

### F6. 정책 기반 백그라운드 제안 — P1

**문제.** periodic 모드는 스냅샷만 쌓고, live 모드 알림은 예산 초과뿐. "열어보니 이미 후보가 계산돼 있다"까지 가면 사용 시간이 몇 분 → 몇 초로 준다.

**설계.** 자동 삭제 없음 — 계산과 알림까지만.

- **정책 모델 (`AppSettings` 확장).** 내장 정책 3종 토글: ① idle ≥ N일(기본 90) 프로젝트의 safe 빌드 산출물, ② safe 룰 후보 합계 ≥ M GiB, ③ 여유 공간 < 예산. 각 정책은 기존 탐지기(`categories`+`idle_days`, `detect_cleanup`) 재사용.
- **실행 경로.** periodic 모드: CLI가 스냅샷 후 `--suggest` 서브커맨드(F8)로 후보 JSON을 `Application Support/space-mesh/suggestions.json`에 기록, 앱 실행 시 로드. live 모드: recompute 후 in-process 평가 → `UNNotification` ("빌드 산출물 8.2 GiB 회수 가능 — 열어서 검토"). 알림 탭 시 앱이 해당 후보를 F1 플랜에 미리 담은 상태로 열림 (체크는 사용자 몫).
- **빈도 가드.** 동일 제안 재알림 금지(후보 집합 해시 기준), 최소 간격 24h.

### F7. 스냅샷 보존 정책 — P2

주기 스냅샷(시간당 최대 24개/일)이 무한 축적된다. `space_index`에 `prune_snapshots(conn, policy)` 추가: 최근 7일 전부 → 8~30일 일별 1개 → 이후 주별 1개 유지, 삭제 후 `VACUUM`은 크기 임계 초과 시만. CLI 저장 경로와 앱 저장 경로 양쪽에서 저장 직후 호출. 설정 화면에 DB 크기·스냅샷 수 표시.

### F8. CLI 확장 — P2

현재 CLI는 스캔/스냅샷/diff만. 추가: `space-mesh detect|dups|advise|suggest --json`. 모두 기존 코어 함수 호출 + serde 직렬화라 저비용. F6 periodic 경로의 전제이자, 스크립팅(CI 디스크 관리)·헤드리스 검증에도 쓰인다. `--json` 스키마는 FFI Record와 필드명을 일치시킨다.

### F9. Full Disk Access 온보딩 — P2

`stats.errors`가 크면 대부분 권한 문제인데 지금은 숫자만 보여준다. 휴리스틱: `~/Library/Mail` 등 FDA 필요 경로 접근 실패 감지 시 배너 → 시스템 설정 딥링크(`x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles`). 에러 카운트를 "권한 거부 n · 기타 m"으로 분해 (scanner `ScanStats`에 `permission_errors` 분리 — errno EACCES/EPERM 카운트).

---

## 5. 마일스톤

| 마일스톤 | 내용 | 완료 정의 |
|---|---|---|
| **M4 — 회수 루프** | F2 증분 재스캔 → F1 통합 플랜/검증 → F9 온보딩 | 플랜 실행 후 측정 회수량이 리포트되고, live 모드 재집계가 증분으로 동작 |
| **M5 — 정확도** | F3 클론 인식/병합, F4 purgeable, F5 mtime, F7 보존 정책 | 클론 쌍 reclaimable=0 표시, 볼륨 게이지에 purgeable 반영, 나이 오버레이 출시 |
| **M6 — 자동화** | F8 CLI 확장 → F6 정책 제안 | periodic 모드에서 24h 내 유효 제안 알림 1회 도달 |

각 마일스톤은 기존 관례를 따른다: 코어 단위 테스트 + `make selftest`에 FFI 신규 표면 케이스 추가 + `make check` 통과. FFI Record 변경 시 `make bindings` 재생성 커밋 포함.

## 6. 횡단 관심사

- **스키마 마이그레이션.** F5가 첫 스키마 변경 — `PRAGMA user_version` 기반 마이그레이션 헬퍼를 `space_index::open`에 도입하고 이후 변경(F1 `reclaim_log`)도 같은 경로를 탄다.
- **동시성.** `ScanHandle`이 가변(F2)이 되면서 Swift 쪽 동시 접근 규약 필요: 조회는 자유, `refresh_paths`는 단일 in-flight (Swift actor로 직렬화).
- **안전 불변식 (전 기능 공통).** ① 모든 삭제는 휴지통 경유 + undo 배치 유지, ② `isSafeToTrash` 가드는 플랜/자동 제안 경로에서도 동일 적용, ③ 백그라운드는 어떤 경우에도 파일을 삭제하지 않는다.
- **성능 예산.** 증분 재스캔 p95 < 1s (변경 1k 파일 기준), live 모드 유휴 CPU < 0.1%, 앱 메모리는 트리 2개(스캔+diff) 상주 기준 현행 유지.

## 7. 열린 질문 (구현 전 결정 필요)

1. F3 클론 감지에서 `F_LOG2PHYS`가 컨테이너 재배치로 위음성을 낼 때, "불확실" 표시의 UX 문구 — 과소 약속(under-promise) 원칙으로 갈 것인가.
2. F6 알림에서 플랜 프리로드 시 warn 항목을 아예 제외할지(현 설계) 포함하되 체크 해제로 둘지.
3. F5 `newest_mtime`이 브라우저 캐시처럼 항상-신선한 디렉토리에서 나이 신호를 무력화함 — 디렉토리 나이를 median으로 볼지 newest로 볼지 벤치 후 결정.
4. iCloud Drive의 dataless 파일(`NSURLUbiquitousItemDownloadingStatus`)은 st_blocks가 0에 가까워 트리맵에서 과소 표시됨 — Phase 2에서 배지만 달지, Phase 3로 미룰지.

## 부록 — 검토했으나 Phase 2에서 제외

- **다중 볼륨/외장 디스크 대시보드**: 스캔 자체는 이미 가능(경로 지정). 볼륨별 스냅샷 UX는 수요 확인 후.
- **git 액션(push/prune) 내장**: 상태 *표시* 도구로서의 신뢰를 지키기 위해 쓰기 작업은 넣지 않는다. "터미널에서 열기" 정도만 M6에 편승 가능.
- **트리맵 파일 타일/줌 애니메이션**: 시각 완성도 항목으로 F5 오버레이와 함께 다루기엔 범위 초과.
- **iCloud 동기화 인식**: 열린 질문 4로 축소.
