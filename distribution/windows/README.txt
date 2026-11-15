Sculith - Windows x64 development preview
https://github.com/SKYRPG8957/Sculith

한국어 빠른 시작
1. ZIP 전체를 쓰기 가능한 새 폴더에 압축 해제하세요.
2. start.cmd를 더블클릭하세요. Rust/Python/Java 설치는 필요 없습니다.
3. 같은 PC의 Minecraft for Windows(Bedrock)에서 서버를 추가하세요.
   주소: 127.0.0.1   포트: 19132
   개발 참고 버전: Bedrock 1.26.60 / protocol 2225. Preview/Beta는 별도입니다.
4. 종료할 때는 서버 창에서 Ctrl+C를 누르세요. 월드를 저장하고 종료합니다.
   창을 강제로 닫으면 시작 이후의 변경이 유실될 수 있습니다.

현재 가능한 테스트: 평지 스폰, 제한된 크리에이티브 이동과 블록 설치/파괴.
공식 Minecraft 클라이언트의 접속·렌더링은 아직 검증하지 못했습니다.
Java는 서버 목록 조회만 가능하고 월드 접속은 아직 구현되지 않았습니다.
일반 인벤토리, 생존/중력, 다른 플레이어 표시, 완전한 리소스팩 호환도 미완료입니다.
이 배포는 완성된 서버나 크로스플레이 지원을 뜻하지 않습니다.

start.cmd는 sculith.toml이 없을 때만 예제를 복사합니다.
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
This is a local, offline-identity development preview. Official Minecraft
client entry/rendering remain unverified; Java world entry is not implemented.
Settings and world data are preserved. No Rust, Java or Python install is needed.

MIT License. Third-party notices and license texts are included.
Independent project, not affiliated with Mojang or Microsoft.
