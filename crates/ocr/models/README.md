PP-OCRv6 detection and recognition models (ONNX) from
https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx and
https://huggingface.co/PaddlePaddle/PP-OCRv6_medium_rec_onnx
(`inference.onnx` / `inference.yml` renamed to `det.*` / `rec.*`).

`det.onnx` is unmodified. In `rec.onnx` (`PP-OCRv6_medium_rec`), 2D/4D `float32`
weight initializers (≥ 1024 elements) are stored as `float16` with standard ONNX
`Cast(to=FLOAT)` nodes (1D biases and normalization parameters stay `float32`),
halving the embedded model size (73 MiB → 36.6 MiB) while `tract` constant-folds
the casts on load with identical recognition output.

Licensed under Apache-2.0 by PaddlePaddle. One recognition model covers 50 languages:
Simplified and Traditional Chinese, English, Japanese and 46 Latin-script languages.

| file     | sha256                                                           |
|----------|------------------------------------------------------------------|
| det.onnx | d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e |
| det.yml  | 193f435274bf9f0b5f71a929bbfbcf148282df7e633b34e7c373e8f44741b516 |
| rec.onnx | f03bdb695a353e7ca7d0dd5ae0d4fa5f2197145c220469e008beb543226624f9 |
| rec.yml  | 991b700facf5b50a7de193468207d5f4255b538dde0d312ae3b7c7a9b6873129 |
