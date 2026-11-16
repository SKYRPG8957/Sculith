# Windows에서 바로 테스트

> 프리뷰 배포본과 한 줄 설치는 중단했습니다. 아래 내용은 기존 배포본의 사용 기록입니다. 현재 실행은 README의 소스 빌드 안내를 사용하세요.

Windows x64용 개발 프리뷰입니다. **공식 베드락 앱의 입장과 Windows 게임 렌더링은 아직 미확인입니다.**
순정 Java 26.3의 로컬 입장·이동·마우스 편집은 [Linux에서 직접 확인](verification/2026-11-16-java26.3.md)했습니다.
개발 대상은 Bedrock `1.26.60 / protocol 2225`와 Java `26.3 / 777`입니다. Preview/Beta는 별도입니다.

1. [테스트 배포](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.16)에서 `windows-x86_64.zip` 파일을 받습니다.
2. ZIP 전체를 새 폴더에 압축 해제하고 `start.cmd`를 더블클릭합니다. Rust·Python·Java 설치는 필요 없습니다.
3. 같은 PC의 Minecraft for Windows에서 **플레이 → 서버 → 서버 추가**로 들어갑니다.
4. 이름은 `Sculith`, 주소는 `127.0.0.1`, 포트는 `19132`로 입력합니다.
   Java **26.3**에서는 멀티플레이 서버 주소에 **`127.0.0.1:25565`**를 입력합니다.
5. 종료할 때 서버 창에서 **Ctrl+C**를 누릅니다. 최종 월드 스냅샷을 저장하고 종료합니다.

설정은 `sculith.toml`, 월드는 `world/region.sculith`입니다. 첫 실행에서만 예제 설정을 복사하며 기존 파일은 보존합니다.
두 에디션은 같은 월드와 승인된 블록 변경을 공유합니다. 이전 설정을 옮긴 경우 Java 접속은 자동으로 켜지지 않습니다.
ZIP의 예제 설정 또는 [Java 개발 경로](java-preview.md)의 `[java]` 항목을 확인하세요.
Preview 16은 Rust 게임 플러그인 `plugins/sculith-gameplay-plugin.exe`도 동봉합니다.
새 설정에서는 접속자에게 개인 환영 메시지를 보내고 승인된 블록 변경을 로그에 기록합니다.
이전 설정에 직접 추가하려면 ZIP 예제의 `[[plugins]]` 항목을 참고하세요. Rust 설치는 필요 없습니다.
Preview 16은 원본 예제 팩을 동봉하고 새 설정의 `[content]`에서 자동 전달합니다.
게임에서 팩 사용 동의가 나타나면 계속을 선택하세요. Stone이 원본의 녹색 체크무늬로 표시됩니다.
파일은 `packs/sculith-demo`, 대응표는 `packs/sculith-demo-textures.json`입니다.
같은 원본이 베드락에 전달되고 명시적으로 지원하는 PNG는 자바 팩으로 변환됩니다.
Preview 16의 새 설정은 `plugins/sculith-forms-plugin.exe`도 로드해 접속 후 안내 메뉴를 엽니다.
베드락 기본 폼과 순정 Java 대화상자에서 선택·취소 결과가 같은 Rust 플러그인으로 돌아갑니다.
한 플레이어에게 한 메뉴만 열고 60초가 지난 응답은 적용하지 않습니다. [폼 API와 실제 화면](forms.md).
Preview 16은 `plugins/sculith-sound-plugin.exe`도 로드해 초기화된 접속자에게 짧은 원본 안내음을 재생합니다.
두 에디션에 같은 OGG를 전달합니다. [API·실제 Java 오디오 출력·제한](sounds.md).
Preview 16은 `plugins/sculith-model-plugin.exe`도 로드해 접속자 앞에 정적 장식 모델을 표시합니다.
[모델 API·원본·실제 Java 화면·제한](models.md)을 확인하세요.
NetherNet은 TCP `19132`에서 신호를 교환하고 임시 UDP 미디어 포트를 사용합니다. HTTP는 loopback 전용이며 미디어는 자기 PC가 소유한 IPv4 주소만 송수신 대상으로 허용합니다.
서명 키 `world/nethernet-identity.pem`은 재시작 시 유지합니다. 업데이트할 때는 서버를 종료하고 `world` 폴더 전체를 보존하세요.
게임이 첫 접속 신뢰 확인을 표시할 수 있습니다. 키 파일은 공유하지 마세요. 자세한 범위는 [NetherNet 문서](nethernet.md)에 기록합니다.
주기적 자동 저장은 아직 없으므로 창을 강제로 닫으면 시작 이후의 변경이 유실될 수 있습니다.

PowerShell에서 설치하려면 새 폴더를 만들 위치에서 다음 명령을 실행할 수도 있습니다.

```powershell
irm https://raw.githubusercontent.com/SKYRPG8957/Sculith/main/scripts/install.ps1 | iex
```

현재 위치에 `Sculith` 폴더를 만들고 SHA-256을 검증한 ZIP을 설치합니다. 같은 이름의 폴더가 있으면 덮어쓰지 않고 멈춥니다.
설치 후 그 폴더의 `start.cmd`를 실행하세요. 설치기는 서버 실행·방화벽 변경·관리자 권한 요청을 자동으로 하지 않습니다.

## 연결이 안 될 때

- 실행 중인 서버를 Ctrl+C로 종료한 뒤 `diagnose.cmd`가 포함된 배포에서는 그것을 실행하고 재접속합니다.
  같은 서버를 두 번 켜면 포트 사용 중 오류(`os error 10048`)가 납니다.
  이전 배포에서는 PowerShell에서 다음과 같이 전송 계층의 진단 로그까지 켤 수 있습니다.

```powershell
$env:RUST_LOG='info,sculith::protocol::bedrock=debug,raknet::server=debug,raknet::session=debug'
.\start.cmd
```

진단 출력의 패킷 종류·크기, 프로토콜 번호와 실패 단계는 원인 구분에 사용합니다. 로그인 요청 원문을 공유할 필요는 없습니다.

- 서버 창의 첫 오류와 게임 버전을 확인합니다. 다른 프로그램이 TCP 또는 UDP `19132`를 쓰고 있으면 충돌합니다.
- 이 설정은 **같은 PC 전용**입니다. 다른 PC/휴대폰에서 접속하거나 `bind`를 공개 주소로 바꾸는 설정이 아닙니다.
- Microsoft의 일부 UWP 앱은 localhost 접속을 기본으로 차단합니다. 해당 패키지가 있는지 일반 PowerShell에서 확인할 수 있습니다.

```powershell
Get-AppxPackage *Minecraft* | Select-Object Name, PackageFamilyName
```

실제 설치된 정식 Minecraft 앱이 UWP loopback 제한을 받는 경우, **관리자 PowerShell**에서 그 앱의 `PackageFamilyName`을 넣어 예외를 추가할 수 있습니다.
다른 앱이나 Preview 패키지를 선택하지 않도록 위 결과를 확인하세요. 아래의 자리표시자를 실제 값으로 바꿔야 합니다.

```powershell
CheckNetIsolation.exe LoopbackExempt -a -n="실제_PackageFamilyName"
```

되돌릴 때는 같은 값으로 `-a` 대신 `-d`를 사용합니다. 데스크톱 앱으로 설치된 버전에는 이 절차가 해당하지 않을 수 있습니다.
loopback 예외가 있어도 아직 미구현된 프로토콜이나 렌더링 동작을 해결해 주는 것은 아닙니다.

문제가 계속되면 Minecraft 버전, ZIP의 `release.json`에 기록된 commit, 서버 창의 오류를 알려주세요.
현재 평지 스폰·제한된 크리에이티브 이동/건축은 헤드리스 프로토콜 클라이언트로 검증했습니다.
기본 상대 플레이어 표시·이동·퇴장은 Preview 15에 포함됩니다. [실제 화면과 검증](verification/2026-11-16-shared-peers.md).
Preview 16은 서버가 정한 시작 아이템·슬롯 변경·빈 손도 전달합니다. [실제 화면과 검증](verification/2026-11-16-shared-equipment.md).
일반 인벤토리, 생존/중력, 계정 스킨·갑옷·보조 손과 완전한 팩 호환성은 아직 구현 중입니다.
