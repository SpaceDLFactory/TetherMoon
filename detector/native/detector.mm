// detector.mm — detector.h의 CoreML 구현 (Obj-C++, ARC 미사용 = 수동 retain/release).
// build(객체): clang++ -std=c++17 -O2 -c detector.mm -o detector.o
#import <Foundation/Foundation.h>
#import <CoreML/CoreML.h>
#include "detector.h"
#include <vector>
#include <algorithm>
#include <cmath>

static const int SIZE = 640, NQ = 300, NC = 80;

struct Detector {
  MLModel *model;
};

Detector *detector_create(const char *path) {
  @autoreleasepool {
    NSError *err = nil;
    NSURL *src = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
    NSURL *compiled = [MLModel compileModelAtURL:src error:&err];
    if (!compiled) { NSLog(@"detector compile: %@", err); return NULL; }
    MLModelConfiguration *cfg = [[MLModelConfiguration alloc] init];
    cfg.computeUnits = MLComputeUnitsAll;
    MLModel *m = [MLModel modelWithContentsOfURL:compiled configuration:cfg error:&err];
    [cfg release];
    if (!m) { NSLog(@"detector load: %@", err); return NULL; }
    Detector *d = new Detector();
    d->model = [m retain];
    return d;
  }
}

static inline float rd(MLMultiArray *m, long q, long k) {
  long off = q * m.strides[1].longValue + k * m.strides[2].longValue;
  const void *p = m.dataPointer;
  switch (m.dataType) {
    case MLMultiArrayDataTypeFloat16: return (float)((const __fp16 *)p)[off];
    case MLMultiArrayDataTypeFloat32: return ((const float *)p)[off];
    case MLMultiArrayDataTypeDouble: return (float)((const double *)p)[off];
    default: return 0;
  }
}

int detector_infer(Detector *d, const uint8_t *rgb, int W, int H,
                   float thr, int max_n,
                   float *ob, float *os, int *oc) {
  if (!d || !rgb || W <= 0 || H <= 0) return -1;
  @autoreleasepool {
    NSError *err = nil;
    MLMultiArray *arr = [[MLMultiArray alloc] initWithShape:@[ @1, @3, @(SIZE), @(SIZE) ]
                                                   dataType:MLMultiArrayDataTypeFloat32 error:&err];
    if (!arr) return -1;
    float *p = (float *)arr.dataPointer;
    // W×H RGB(HWC) → 640² bilinear, /255, NCHW
    for (int y = 0; y < SIZE; y++) {
      float sy = (y + 0.5f) * H / SIZE - 0.5f;
      int y0 = (int)floorf(sy); float fy = sy - y0;
      int y0c = std::min(std::max(y0, 0), H - 1), y1c = std::min(std::max(y0 + 1, 0), H - 1);
      for (int x = 0; x < SIZE; x++) {
        float sx = (x + 0.5f) * W / SIZE - 0.5f;
        int x0 = (int)floorf(sx); float fx = sx - x0;
        int x0c = std::min(std::max(x0, 0), W - 1), x1c = std::min(std::max(x0 + 1, 0), W - 1);
        for (int c = 0; c < 3; c++) {
          float v00 = rgb[(y0c * W + x0c) * 3 + c], v01 = rgb[(y0c * W + x1c) * 3 + c];
          float v10 = rgb[(y1c * W + x0c) * 3 + c], v11 = rgb[(y1c * W + x1c) * 3 + c];
          float v = (v00 * (1 - fx) + v01 * fx) * (1 - fy) + (v10 * (1 - fx) + v11 * fx) * fy;
          p[(c * SIZE + y) * SIZE + x] = v / 255.0f;
        }
      }
    }
    MLDictionaryFeatureProvider *in =
        [[MLDictionaryFeatureProvider alloc] initWithDictionary:@{@"image" : [MLFeatureValue featureValueWithMultiArray:arr]}
                                                          error:&err];
    id<MLFeatureProvider> out = [d->model predictionFromFeatures:in error:&err];
    [arr release]; [in release];
    if (!out) { NSLog(@"detector predict: %@", err); return -1; }

    MLMultiArray *lg = [out featureValueForName:@"logits"].multiArrayValue;  // [1,300,80]
    MLMultiArray *bx = [out featureValueForName:@"boxes"].multiArrayValue;   // [1,300,4]

    struct Det { float s; int c, q; };
    std::vector<Det> ds;
    for (int q = 0; q < NQ; q++) {
      float best = -1; int bc = 0;
      for (int c = 0; c < NC; c++) {
        float s = 1.0f / (1.0f + expf(-rd(lg, q, c)));
        if (s > best) { best = s; bc = c; }
      }
      if (best > thr) ds.push_back({best, bc, q});
    }
    std::sort(ds.begin(), ds.end(), [](const Det &a, const Det &b) { return a.s > b.s; });

    int n = std::min((int)ds.size(), max_n);
    for (int i = 0; i < n; i++) {
      const Det &e = ds[i];
      float cx = rd(bx, e.q, 0), cy = rd(bx, e.q, 1), w = rd(bx, e.q, 2), h = rd(bx, e.q, 3);
      ob[i * 4 + 0] = (cx - w / 2) * W;
      ob[i * 4 + 1] = (cy - h / 2) * H;
      ob[i * 4 + 2] = (cx + w / 2) * W;
      ob[i * 4 + 3] = (cy + h / 2) * H;
      os[i] = e.s;
      oc[i] = e.c;
    }
    return n;
  }
}

void detector_free(Detector *d) {
  if (d) { [d->model release]; delete d; }
}
