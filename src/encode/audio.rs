//! Shared audio-branch construction for the three video encode pipelines
//! (`VideoSession`, `VideoSessionDmabuf`, `VideoSessionDmabufMulti`).
//!
//! Two independent bits control capture: `mic` (PulseAudio default
//! *source*, typically the mic) and `system` (PulseAudio
//! `@DEFAULT_MONITOR@`, i.e. the monitor of the current default sink).
//! Both bits set mixes the two through `audiomixer` before encoding.
//!
//! On a PipeWire system the Pulse compat layer resolves `pulsesrc` and
//! `@DEFAULT_MONITOR@` the same way — no PipeWire-specific element
//! needed for this baseline. Per-app capture would require
//! `pipewiresrc target-object=<node-id>`; that lives behind a separate
//! flag if/when it lands.

use anyhow::{Context, Result};
use gstreamer::prelude::*;

/// Audio-branch fragment for `parse_launch`-based pipelines. Empty
/// string when neither bit is set. The trailing element name `mux` is
/// expected to be the muxer's `name=mux` in the surrounding pipeline.
///
/// A `queue` immediately after each `pulsesrc` absorbs the burst of
/// samples that arrives while the rest of the pipeline is still
/// preroll-ing. Without it, pulsesrc's internal buffer overflows in the
/// first ~100 ms (~9k samples) before downstream is ready, which spams
/// "Can't record audio fast enough" warnings and creates a tiny gap at
/// the start of the recording.
pub fn audio_branch_str(mic: bool, system: bool) -> String {
    match (mic, system) {
        (false, false) => String::new(),
        (true, false) => {
            "pulsesrc ! queue ! audioconvert ! audioresample \
             ! avenc_aac ! mux.audio_0"
                .into()
        }
        (false, true) => {
            "pulsesrc device=@DEFAULT_MONITOR@ ! queue ! audioconvert ! audioresample \
             ! avenc_aac ! mux.audio_0"
                .into()
        }
        (true, true) => {
            // Two pulsesrcs feeding an audiomixer named `amix`. The mixer
            // resamples/converts internally to a common format, so the
            // post-mix audioconvert+audioresample mostly just adapt for
            // avenc_aac's preferred caps.
            "pulsesrc ! queue ! amix.sink_0 \
             pulsesrc device=@DEFAULT_MONITOR@ ! queue ! amix.sink_1 \
             audiomixer name=amix \
               ! audioconvert ! audioresample ! avenc_aac ! mux.audio_0"
                .into()
        }
    }
}

/// Programmatic equivalent for pipelines built element-by-element (used
/// by `VideoSessionDmabufMulti`). Adds the requested chain to `pipeline`
/// and links the encoder's output into a fresh `audio_%u` request pad on
/// `muxer`. No-op when both bits are false.
pub fn add_audio_branch(
    pipeline: &gstreamer::Pipeline,
    muxer: &gstreamer::Element,
    mic: bool,
    system: bool,
) -> Result<()> {
    use gstreamer::ElementFactory;

    if !mic && !system {
        return Ok(());
    }

    // Each pulsesrc gets its own immediate `queue` to absorb startup
    // bursts — same rationale as in `audio_branch_str`.
    let make_mic_chain = || -> Result<(gstreamer::Element, gstreamer::Element)> {
        let src = ElementFactory::make("pulsesrc")
            .build()
            .context("make pulsesrc (mic)")?;
        let q = ElementFactory::make("queue").build()?;
        Ok((src, q))
    };
    let make_sys_chain = || -> Result<(gstreamer::Element, gstreamer::Element)> {
        let src = ElementFactory::make("pulsesrc")
            .property("device", "@DEFAULT_MONITOR@")
            .build()
            .context("make pulsesrc (system monitor)")?;
        let q = ElementFactory::make("queue").build()?;
        Ok((src, q))
    };
    let aconv = ElementFactory::make("audioconvert").build()?;
    let aresample = ElementFactory::make("audioresample").build()?;
    let aenc = ElementFactory::make("avenc_aac").build()?;

    match (mic, system) {
        (true, false) => {
            let (src, q) = make_mic_chain()?;
            pipeline.add_many([&src, &q, &aconv, &aresample, &aenc])?;
            gstreamer::Element::link_many([&src, &q, &aconv, &aresample, &aenc])
                .context("link mic audio chain")?;
        }
        (false, true) => {
            let (src, q) = make_sys_chain()?;
            pipeline.add_many([&src, &q, &aconv, &aresample, &aenc])?;
            gstreamer::Element::link_many([&src, &q, &aconv, &aresample, &aenc])
                .context("link system audio chain")?;
        }
        (true, true) => {
            let (mic_src, mic_q) = make_mic_chain()?;
            let (sys_src, sys_q) = make_sys_chain()?;
            let mixer = ElementFactory::make("audiomixer").build()?;
            pipeline.add_many([
                &mic_src, &mic_q, &sys_src, &sys_q, &mixer, &aconv, &aresample, &aenc,
            ])?;
            // Each source → its own queue, then link the queue's src pad
            // into a fresh sink_%u on the mixer.
            for (idx, (src, q)) in [(&mic_src, &mic_q), (&sys_src, &sys_q)].iter().enumerate() {
                gstreamer::Element::link_many([*src, *q])
                    .with_context(|| format!("link pulsesrc {idx} → queue"))?;
                let q_src = q
                    .static_pad("src")
                    .ok_or_else(|| anyhow::anyhow!("queue has no src pad"))?;
                let sink = mixer
                    .request_pad_simple("sink_%u")
                    .ok_or_else(|| anyhow::anyhow!("audiomixer rejected sink_%u"))?;
                q_src
                    .link(&sink)
                    .with_context(|| format!("link queue {idx} → audiomixer"))?;
            }
            gstreamer::Element::link_many([&mixer, &aconv, &aresample, &aenc])
                .context("link audiomixer tail")?;
        }
        (false, false) => unreachable!(),
    }

    let aenc_src = aenc
        .static_pad("src")
        .ok_or_else(|| anyhow::anyhow!("avenc_aac has no src pad"))?;
    let mux_audio = muxer
        .request_pad_simple("audio_%u")
        .ok_or_else(|| anyhow::anyhow!("muxer rejected audio_%u request pad"))?;
    aenc_src
        .link(&mux_audio)
        .context("link avenc_aac → mux.audio_0")?;
    Ok(())
}
