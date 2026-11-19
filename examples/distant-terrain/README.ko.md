# 스컬리스 베드락 원거리 지형

구동기 폴더에 `distant-terrain` 폴더를 두고, 서버를 끈 상태에서 `sculith.toml`에 아래 설정을 추가한 뒤 서버를 시작하세요. 윈도우에서는 `Settings.cmd`로 설정 파일을 열 수 있습니다.

```toml
[resource_pack.shared]
source = "distant-terrain/pack"
mapping = "distant-terrain/mapping.json"
required = true
```

이미 공유 리소스팩을 쓰는 서버에서는 기존 팩에 여섯 모델의 텍스처·지오메트리·엔티티·렌더 컨트롤러 파일을 복사하고, 기존 매핑의 `models` 배열에 `mapping.json`의 항목을 추가하세요. 팩 매니페스트는 기존 것을 사용하세요.

팩을 받은 베드락 플레이어에게 일반 시야 밖의 지형을 8·16·32블록 타일로 표시합니다. 각 타일은 표면 높이와 재질을 요약한 평평한 모델입니다. 생성된 지형을 백그라운드에서 읽고, 차원별 `sculith-lod` 폴더에 요약본을 저장합니다. 시뮬레이션 거리나 몹 수는 그대로 설정할 수 있습니다.
