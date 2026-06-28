// detector.h — RT-DETR CoreML 검출기의 pure-C ABI.
// bindgen이 그대로 Rust로 가져갈 수 있는 최소 인터페이스. (.mm 구현은 detector.mm)
#ifndef TETHERMOON_DETECTOR_H
#define TETHERMOON_DETECTOR_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Detector Detector;

// .mlpackage 경로로 모델 로드(컴파일+ANE/GPU). 실패 시 NULL.
Detector *detector_create(const char *mlpackage_path);

// 추론. rgb = HWC uint8 (width*height*3), 임의 크기 — 내부에서 모델 입력(640²)으로 bilinear 리사이즈.
// 결과는 호출자가 할당한 배열(길이 max_n)에 채움. 반환 = 검출 수(0..max_n), 오류 시 -1.
//   out_boxes:   max_n*4  (x0,y0,x1,y1, 원본 픽셀 좌표)
//   out_scores:  max_n
//   out_classes: max_n    (COCO 0..79)
int detector_infer(Detector *d, const uint8_t *rgb, int width, int height,
                   float score_thresh, int max_n,
                   float *out_boxes, float *out_scores, int *out_classes);

void detector_free(Detector *d);

#ifdef __cplusplus
}
#endif

#endif
