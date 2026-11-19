# 스컬리스 길드 의뢰

게임에서 `/guild`를 입력하면 산등성이 정찰 의뢰가 열립니다. 의뢰 수락은 캐릭터에 한 번 저장되고 내 의뢰에서 다시 볼 수 있습니다. 닫기 또는 Esc로 돌아갑니다.

## 베드락 원본

화면은 `bedrock/ui/server_form.json`에 정의되어 있습니다. 제목이 `길드 의뢰`인 액션 폼이 `server_form_factory`의 `long_form`을 통해 `guild_panel`을 엽니다. 배경, 제목, 지도, 설명은 네 개의 이미지 컨트롤입니다. `form_buttons` 컬렉션은 버튼 셋을 가로로 배치합니다. 기본, 포커스, 누름 그림과 텍스트 바인딩이 이 파일에 있습니다. 버튼 입력은 베드락의 `button.form_button_click`으로 돌아갑니다.

## 자바 화면

Minecraft Java 26.3의 Fabric 프로필에 Fabric API 0.166.2+26.3과 Sculith Companion을 설치합니다. 서버가 보내는 자바 리소스팩에 베드락 JSON과 PNG가 그대로 들어갑니다. Companion은 현재 폼의 제목과 서버가 발급한 액션을 확인한 뒤 Fabric Screen API로 같은 좌표에 그림과 클릭 영역을 배치합니다. 키보드 포커스와 Esc도 사용할 수 있습니다. Companion을 설치하지 않은 자바 클라이언트에는 스컬리스의 기본 선택 폼이 열립니다.

현재 Companion이 읽는 범위는 중앙 정렬된 고정 픽셀 패널, 이미지, 가로·세로 액션 버튼 컬렉션, 제목·버튼 텍스트 일치 바인딩과 버튼 상태 세 가지입니다. HUD, 애니메이션, 임의 표현식은 이 예제의 지원 범위를 벗어납니다.

## 서버 설정

예제의 `bedrock` 폴더와 `mapping.json`을 서버의 `resource_packs/guild` 아래에 놓고 `sculith.toml`에 다음 설정을 적용합니다. 공유 팩을 켜면 스컬리스가 베드락에 원본 팩을 내려주고 자바에는 같은 파일을 포함한 팩을 게임 포트에서 제공합니다.

```toml
[resource_pack.shared]
source = "resource_packs/guild/bedrock"
mapping = "resource_packs/guild/mapping.json"
required = true

[plugins.overrides.sculith-guild]
allow_unsigned = true
```

## 빌드

`python build-packs.py dist`를 실행하면 `guild.mcpack`, `guild-java.zip`, 팩 UUID와 해시를 담은 `packs.json`이 생깁니다. JSON 안의 지도 좌표나 버튼 간격을 수정하고 팩을 다시 만들면 두 에디션에 같은 변경을 적용할 수 있습니다. 재배포할 때는 베드락 manifest의 헤더와 모듈 버전도 올립니다.

플러그인은 이 디렉터리에서 `cargo build --manifest-path plugin/Cargo.toml --locked --release --target wasm32-wasip2`로 빌드합니다. 생성된 `sculith_guild.wasm`을 스컬리스 서버의 `plugins`에 넣습니다.

공식 베드락 폼과 자바 화면 API의 기준 버전을 확인할 수 있습니다. [Mojang 정식 샘플](https://github.com/Mojang/bedrock-samples/releases/tag/v1.26.60.5) · [Bedrock Wiki 폼 제작](https://wiki.bedrock.dev/json-ui/modifying-server-forms.html) · [Fabric API](https://modrinth.com/mod/fabric-api/versions?g=26.3)
