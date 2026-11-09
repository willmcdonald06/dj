// C interface to Signalsmith Stretch for the dj-stretch crate. Planar buffers:
// `inputs[c]` / `outputs[c]` point to one channel each.
#include "signalsmith-stretch.h"

using Stretch = signalsmith::stretch::SignalsmithStretch<float>;

extern "C" {

void *dj_stretch_create(int channels, float sample_rate, int cheaper, int split) {
    auto *stretch = new Stretch();
    if (cheaper) {
        stretch->presetCheaper(channels, sample_rate, split != 0);
    } else {
        stretch->presetDefault(channels, sample_rate, split != 0);
    }
    return stretch;
}

void dj_stretch_destroy(void *handle) { delete static_cast<Stretch *>(handle); }

void dj_stretch_reset(void *handle) { static_cast<Stretch *>(handle)->reset(); }

int dj_stretch_input_latency(void *handle) {
    return static_cast<Stretch *>(handle)->inputLatency();
}

int dj_stretch_output_latency(void *handle) {
    return static_cast<Stretch *>(handle)->outputLatency();
}

int dj_stretch_block_samples(void *handle) {
    return static_cast<Stretch *>(handle)->blockSamples();
}

int dj_stretch_interval_samples(void *handle) {
    return static_cast<Stretch *>(handle)->intervalSamples();
}

void dj_stretch_set_transpose(void *handle, float factor) {
    static_cast<Stretch *>(handle)->setTransposeFactor(factor);
}

void dj_stretch_seek(void *handle, const float *const *inputs, int frames, double rate) {
    static_cast<Stretch *>(handle)->seek(inputs, frames, rate);
}

void dj_stretch_process(void *handle, const float *const *inputs, int input_frames,
                        float *const *outputs, int output_frames) {
    static_cast<Stretch *>(handle)->process(inputs, input_frames, outputs, output_frames);
}

}
