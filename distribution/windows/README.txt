Sculith - Windows x64 development preview
https://github.com/SKYRPG8957/Sculith

한국어 빠른 시작
1. ZIP 전체를 쓰기 가능한 새 폴더에 압축 해제하세요.
2. start.cmd를 더블클릭하세요. Rust/Python/Java 설치는 필요 없습니다.
3. 같은 PC의 Minecraft for Windows(Bedrock)에서 서버를 추가하세요.
   주소: 127.0.0.1   포트: 19132
   개발 참고 버전: Bedrock 1.26.60 / protocol 2225. Preview/Beta는 별도입니다.
   Java 26.3에서는 멀티플레이 서버 주소에 127.0.0.1:25565를 입력하세요.
   두 에디션은 하나의 월드와 승인된 블록 변경을 공유합니다.
4. 종료할 때는 서버 창에서 Ctrl+C를 누르세요. 월드를 저장하고 종료합니다.
   창을 강제로 닫으면 시작 이후의 변경이 유실될 수 있습니다.

현재 가능한 테스트: 평지 스폰, 제한된 크리에이티브 이동과 블록 설치/파괴.
Rust 예제 플러그인이 동봉돼 접속한 플레이어에게 환영 메시지를 보내고 블록 변경을 기록합니다.
파일은 plugins/sculith-gameplay-plugin.exe이며, sculith.toml의 [[plugins]]에서 등록합니다.
기존 설정을 옮겼다면 예제 플러그인도 자동으로 추가하지 않습니다.
공유 메뉴 예제 plugins/sculith-forms-plugin.exe도 동봉합니다. 새 설정은 접속 후 안내 메뉴를 엽니다.
베드락은 기본 폼, 순정 Java 26.3은 대화상자로 같은 제목·내용·선택지를 표시합니다.
버튼 선택 또는 취소 결과는 메뉴를 연 Rust 플러그인에만 돌아가며 개인 안내 메시지를 보냅니다.
한 플레이어에게 하나의 메뉴만 열고, 60초가 지난 응답은 적용하지 않습니다.
plugins/sculith-sound-plugin.exe는 공통 팩의 짧은 원본 안내음을 접속자에게만 재생합니다.
같은 OGG를 두 에디션이 받으며, 순정 Java의 실제 오디오 출력을 확인했습니다.
plugins/sculith-model-plugin.exe는 접속자 앞에 원본 팩의 정적 장식 모델을 표시합니다.
플레이어별 개인 미리보기이며 이동·충돌·애니메이션을 제공하지 않습니다.
단순 직육면체 모델과 원본 텍스처의 실제 순정 Java 표시를 확인했습니다.
새 설정은 동봉한 베드락 예제 팩을 두 에디션에 자동 전달합니다. 게임의 팩 사용 동의가 필요할 수 있습니다.
Stone에 원본 체크무늬 텍스처가 표시됩니다. 순정 Java 26.3에서 실제 다운로드와 표시를 확인했습니다.
팩은 packs/sculith-demo, 변환 매핑은 packs/sculith-demo-textures.json입니다.
기존 설정에 팩을 추가하려면 예제의 [content] 항목을 참고하세요. 다른 자산의 변환은 아직 제한됩니다.
순정 Java 26.3의 로컬 입장·이동·마우스 건축은 Linux에서 확인했습니다.
공식 베드락 앱의 입장과 Windows 게임 렌더링은 아직 확인하지 못했습니다.
Java 26.3의 로컬 로그인·평지 전송·제한된 건축 경로도 구현했습니다.
초기화된 상대 플레이어의 기본 이름·아바타·이동·퇴장을 표시합니다.
일반 인벤토리, 생존/중력, 계정 스킨·장비 전달, 완전한 리소스팩 호환은 미완료입니다.
이 배포는 완성된 서버나 크로스플레이 지원을 뜻하지 않습니다.

start.cmd는 sculith.toml이 없을 때만 예제를 복사합니다.
기존 설정을 옮기면 Java 접속은 자동 활성화되지 않습니다. 새 예제의 [java] 항목을 확인하세요.
설정은 sculith.toml, 월드는 world/region.sculith에 저장합니다.
NetherNet 연결용 서명 키는 world/nethernet-identity.pem에 저장하고 재시작 시 유지합니다.
게임에서 첫 접속 신뢰 확인이 나타날 수 있습니다. 키 파일은 공유하지 마세요.
기존 설정과 월드 파일을 삭제하지 마세요. 업데이트는 새 폴더에 압축을 풀어 옮기세요.
테스트 신원은 같은 PC의 loopback에서만 허용됩니다. 공개 바인딩으로 바꾸면 거부됩니다.
네트워크 방화벽 설정을 바꾸거나 관리자 권한으로 실행할 필요는 없습니다.

접속 문제가 있으면:
- 서버를 Ctrl+C로 종료한 뒤 diagnose.cmd를 실행하고 접속을 한 번 재시도하세요.
  초기 UDP 패킷, NetherNet 신호/연결과 로그인 실패 단계가 서버 창에 표시됩니다.
  diagnose.cmd는 저장된 설정이나 Windows 방화벽 설정을 바꾸지 않습니다.
- ZIP 안에서 직접 실행하지 말고 전체 압축을 해제했는지 확인하세요.
- 서버 창의 첫 오류를 확인하세요. 다른 서버가 19132/TCP 또는 UDP를 쓰면 먼저 종료하세요.
- 버전을 확인하세요. 지원되지 않는 프로토콜은 거부됩니다.
- Windows의 일부 Minecraft 앱은 UWP loopback 제한을 적용합니다.
  https://github.com/SKYRPG8957/Sculith/blob/main/docs/windows-quickstart.md
  의 loopback 안내를 확인하세요. 이 실행기는 해당 Windows 설정을 자동으로 변경하지 않습니다.
- 게임 버전, release.json의 commit, 서버 로그와 증상을 알려주세요.
  토큰·서명된 로그인 데이터·개인 정보를 공개 이슈에 넣지 마세요.

English
Extract the entire ZIP, then double-click start.cmd. Add 127.0.0.1:19132 in
Bedrock on the same PC. Ctrl+C saves the world and stops the server.
This is a local, offline-identity development preview. Unmodified Java 26.3
entry, movement and building were checked on Linux; official Bedrock entry
and Windows game rendering remain unverified. Java uses 127.0.0.1:25565 and
the same authoritative world. Other versions and online Java authentication
are not implemented. Existing settings are not automatically opted in.
Settings and world data are preserved. No Rust, Java or Python install is needed.
The bundled Rust gameplay plugin sends a private welcome and observes committed
block changes. It is enabled in new example settings; existing settings are preserved.
The forms plugin also opens a shared menu through a Bedrock form or vanilla Java
dialog. Selection/cancellation returns only to its owning plugin. One form per
player is pending at a time; replies expire after 60 seconds. This is a simple-form
fallback, not arbitrary Bedrock JSON UI conversion.
The model plugin also shows one private static cuboid model from the authored pack.
Stock Java geometry/texture rendering was checked; animation and general entities are pending.
The sound plugin privately plays an original short tone from the same OGG in both packs.
Stock Java PCM output was verified; this does not verify the official Bedrock app.
New settings also deliver the bundled Bedrock texture/sound example to both editions;
Java receives the mapped PNG conversion. Accept the game's pack prompt when shown.
Stone displays the original checkerboard. General sound/model/UI conversion is pending.

MIT License. Third-party notices and license texts are included.
Independent project, not affiliated with Mojang or Microsoft.
