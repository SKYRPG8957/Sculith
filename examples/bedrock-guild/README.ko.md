# 스컬리스 길드 의뢰

게임에서 `/guild`를 입력하면 산등성이 정찰 의뢰가 열립니다. 의뢰 수락은 캐릭터에 한 번 저장되고 내 의뢰에서 다시 볼 수 있습니다. 닫기 또는 Esc로 돌아갑니다.

## 베드락 원본

화면은 `bedrock/ui/server_form.json`에 정의되어 있습니다. 제목이 `길드 의뢰`인 액션 폼이 `server_form_factory`의 `long_form`을 통해 `guild_panel`을 엽니다. 배경, 제목, 지도, 설명은 네 개의 이미지 컨트롤입니다. `form_buttons` 컬렉션은 버튼 셋을 가로로 배치합니다. 기본, 포커스, 누름 그림과 텍스트 바인딩이 이 파일에 있습니다. 버튼 입력은 베드락의 `button.form_button_click`으로 돌아갑니다.

## 자바 바닐라 26.3

플레이어는 서버 리소스팩을 수락하고 `/guild`를 입력합니다. 지도와 한글 버튼을 클릭하면 두 에디션에서 같은 길드 플러그인이 의뢰를 처리합니다.

빌드할 때 베드락 JSON에서 패널 크기, 이미지 위치, 버튼 영역을 읽어 자바 비트맵 글꼴과 대화창을 만듭니다. 서버가 팩을 한 번 전송하고 메뉴를 열 때 대화창을 보냅니다. 자바 기본 클라이언트는 화면 위에 서버 대화창 안내 아이콘을 표시합니다. 베드락의 포커스·누름 그림 전환은 바닐라 자바에서 정지 이미지로 표시됩니다.

## 서버에 적용

공유 팩을 지원하는 스컬리스에서 `shared/bedrock`과 `shared/mapping.json`을 `resource_packs/guild/` 아래에 놓습니다. `sculith-guild.wasm`을 서버의 `plugins/`에 넣고 `sculith.toml`의 해당 설정을 적용합니다. 에디션별 팩 설정은 기본값으로 둡니다. 두 에디션에 대한 접속을 허용하려면 기존 `[networking.java]`에서 `enabled = true`를 설정합니다.

```toml
[resource_pack.shared]
source = "resource_packs/guild/bedrock"
mapping = "resource_packs/guild/mapping.json"
required = true

[plugins.overrides.sculith-guild]
allow_unsigned = true
```

`form_fonts` 매핑이 지원되는 버전에서는 스컬리스가 자바 접속 포트로 팩도 제공합니다. 이전 구동기에서 에디션별 팩을 운영할 때는 `guild-java.zip`의 다운로드 주소와 `packs.json`의 SHA-1을 `[resource_pack.java]`에, `guild.mcpack`과 해당 UUID·버전·크기를 `[resource_pack.bedrock]`에 등록합니다.

## 원본에서 다시 빌드

빌드 환경에 Python과 Pillow가 필요합니다. 이 디렉터리에서 다음 명령을 실행합니다.

```sh
python build-packs.py --vanilla dist
cp dist/guild-dialog.json plugin/src/dialog-template.json
cargo build --manifest-path plugin/Cargo.toml --locked --release --target wasm32-wasip2
```

팩과 플러그인을 함께 교체합니다. 새 팩을 배포할 때는 베드락 manifest의 헤더와 모듈 버전도 올립니다. 기존 Sculith Companion용 폼을 유지하는 서버는 `--features companion`으로 플러그인을 빌드하고 기존 팩 매핑을 사용합니다.

기준 자료는 Mojang의 정식 샘플과 Bedrock Wiki의 서버 폼 문서입니다. [Mojang 샘플](https://github.com/Mojang/bedrock-samples/releases/tag/v1.26.60.5) · [서버 폼 제작](https://wiki.bedrock.dev/json-ui/modifying-server-forms.html)
