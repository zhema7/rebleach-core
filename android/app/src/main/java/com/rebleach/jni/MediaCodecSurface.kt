package com.rebleach.jni

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import android.view.SurfaceView
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Android 主控端视频预览：MediaCodec H.264 解码 → SurfaceView 渲染。
 *
 * 用法：MediaCodecSurface(handle = myHandle, isRunning = connected)
 *
 * 内部流程：
 *   1. AndroidView 创建 SurfaceView
 *   2. SurfaceHolder.Callback.surfaceCreated 拿到 Surface
 *   3. 创建 MediaCodec H.264 解码器 → configure(surface) → start()
 *   4. 后台协程循环：NativeBridge.pollFrame() 拉帧 → queueInputBuffer()
 *      同时 drain: dequeueOutputBuffer → releaseOutputBuffer(render=true)
 *   5. isRunning=false 时 stop/release codec
 *
 * 性能目标：1080p30 稳定解码不丢帧（MediaCodec 硬编硬解，现代 Android 没问题）。
 */
@Composable
fun MediaCodecSurface(
    handle: Long,
    isRunning: Boolean,
    modifier: Modifier = Modifier,
    onStatsUpdate: ((fps: Float, kbps: Float, frames: Long) -> Unit)? = null,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    var surface by remember { mutableStateOf<Surface?>(null) }
    var codec by remember { mutableStateOf<MediaCodec?>(null) }
    var decodeJob by remember { mutableStateOf<Job?>(null) }
    var inputJob by remember { mutableStateOf<Job?>(null) }

    // Surface 容器
    AndroidView(
        modifier = modifier.fillMaxSize(),
        factory = { ctx ->
            SurfaceView(ctx).apply {
                holder.addCallback(object : android.view.SurfaceHolder.Callback {
                    override fun surfaceCreated(h: android.view.SurfaceHolder) {
                        surface = h.surface
                    }
                    override fun surfaceChanged(holder: android.view.SurfaceHolder, fmt: Int, width: Int, height: Int) {}
                    override fun surfaceDestroyed(h: android.view.SurfaceHolder) {
                        surface = null
                    }
                })
            }
        }
    )

    // isRunning 或 surface 变化时：启动/停止解码
    LaunchedEffect(isRunning, surface) {
        if (!isRunning || surface == null) {
            // 停止
            inputJob?.cancel(); inputJob = null
            decodeJob?.cancel(); decodeJob = null
            codec?.stop(); codec?.release(); codec = null
            return@LaunchedEffect
        }

        val surf = surface!!
        Log.i("MediaCodecSurface", "创建 H.264 解码器")

        // 创建 H.264 解码器
        val mc = try {
            MediaCodec.createDecoderByType("video/avc")
        } catch (e: IllegalArgumentException) {
            Log.e("MediaCodecSurface", "设备不支持 H.264 解码", e)
            return@LaunchedEffect
        }

        val fmt = MediaFormat.createVideoFormat("video/avc", 1280, 720).apply {
            setInteger(MediaFormat.KEY_FRAME_RATE, 30)
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
        }

        mc.configure(fmt, surf, null, 0)
        mc.start()
        codec = mc

        val scope = this
        val inputBuf = ByteArray(512 * 1024) // 512KB，够塞绝大多数帧
        val outputInfo = MediaCodec.BufferInfo()

        // === 输入线程：pollFrame → queueInputBuffer ===
        inputJob = scope.launch(Dispatchers.IO) {
            var framesIn = 0L
            var bytesIn = 0L
            var lastStat = System.currentTimeMillis()
            while (isActive) {
                val isKeyPtr = byteArrayOf(0)
                val n = NativeBridge.pollFrame(handle, inputBuf, inputBuf.size, isKeyPtr)
                if (n > 0) {
                    val isKey = isKeyPtr[0] != 0.toByte()
                    val mcLocal = mc
                    try {
                        val idx = mcLocal.dequeueInputBuffer(10_000)
                        if (idx >= 0) {
                            val buf = mcLocal.getInputBuffer(idx)!!
                            buf.clear()
                            buf.put(inputBuf, 0, n)
                            buf.flip()
                            var flags = 0
                            if (isKey) flags = flags or MediaCodec.BUFFER_FLAG_KEY_FRAME
                            mcLocal.queueInputBuffer(idx, 0, n, 0, flags)
                            framesIn++; bytesIn += n

                            // SPS/PPS 检测：Annex-B 前 4 字节如果是 00 00 00 01 + 67/68，自动加 BUFFER_FLAG_CODEC_CONFIG
                            // MediaCodec 能从 annex-b 自动识别
                        }
                    } catch (t: MediaCodec.CryptoException) {
                        Log.e("MediaCodecSurface", "queueInputBuffer crypto", t)
                    } catch (t: IllegalStateException) {
                        // codec 已 stop，退出
                        break
                    }
                } else if (n == 0) {
                    // 队空，稍等再拉
                    delay(3)
                } else {
                    // -1: handle 不在，退出
                    break
                }

                // 每 1 秒上报统计
                val now = System.currentTimeMillis()
                if (now - lastStat >= 1000) {
                    val dt = (now - lastStat) / 1000.0f
                    val fps = framesIn / dt
                    val kbps = (bytesIn * 8 / dt / 1000.0f)
                    onStatsUpdate?.invoke(fps, kbps, framesIn)
                    Log.d("MediaCodecSurface", "IN fps=${"%.1f".format(fps)} kbps=${"%.0f".format(kbps)} frames=$framesIn")
                    framesIn = 0; bytesIn = 0; lastStat = now
                }
            }
        }

        // === 输出线程：drain → releaseOutputBuffer(render=true) ===
        decodeJob = scope.launch(Dispatchers.IO) {
            var framesOut = 0L
            while (isActive) {
                val mcLocal = mc
                try {
                    val idx = mcLocal.dequeueOutputBuffer(outputInfo, 10_000)
                    when {
                        idx >= 0 -> {
                            framesOut++
                            mcLocal.releaseOutputBuffer(idx, true) // render to Surface
                        }
                        idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                            Log.i("MediaCodecSurface", "解码器格式变更: ${mcLocal.outputFormat}")
                        }
                        idx == MediaCodec.INFO_TRY_AGAIN_LATER -> {
                            delay(2)
                        }
                    }
                } catch (e: IllegalStateException) {
                    break // codec stop
                }
            }
            Log.i("MediaCodecSurface", "解码结束，共渲染 $framesOut 帧")
        }
    }
}
