package com.rebleach.jni

import android.util.Log
import kotlinx.coroutines.*
import java.nio.charset.StandardCharsets

/** Kotlin ↔ Rust 核心桥接层。
 *
 * 所有 native 方法签名必须严格对齐 client/src/jni.rs 的 #[unsafe(no_mangle)] extern "system" 函数。
 * 字符串统一用 ByteArray + UTF-8（JNI 对 String 做编码转换有坑，直接传 bytes 更稳）。
 *
 * 线程模型：
 *   - start() 在调用线程（通常是 Dispatchers.Main）
 *   - pollEventLoop() 用 Dispatchers.IO 后台线程 ~30fps 轮询事件队列
 *   - Rust 侧 push_video_frame / push_audio_frame 从 Kotlin 后台线程调用（MediaCodec callback）
 */
object NativeBridge {

    private const val TAG = "NativeBridge"
    private const val BUF_SIZE = 65536 // 64KB 足够装下大部分 JSON 事件
    private const val POLL_INTERVAL_MS = 33L // ~30fps

    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    @Volatile private var libLoaded = false
    @Volatile private var loadError: String? = null

    /** 延迟 loadLibrary——避免 object init 块同步崩溃。
     *  首次调用任何 native 方法前都会自动调这个。 */
    private fun ensureLoaded(): Boolean {
        if (libLoaded) return true
        val err = loadError
        if (err != null) return false
        synchronized(this) {
            if (libLoaded) return true
            if (loadError != null) return false
            try {
                // 先尝试 debuggable=false 路径
                System.loadLibrary("rebleach_client")
                libLoaded = true
                Log.i(TAG, "librebleach_client.so 已加载")
                return true
            } catch (e: UnsatisfiedLinkError) {
                loadError = e.message ?: "loadLibrary failed"
                Log.e(TAG, "loadLibrary 失败", e)
                return false
            } catch (e: Throwable) {
                loadError = e.message ?: "unknown error"
                Log.e(TAG, "loadLibrary 未知异常", e)
                return false
            }
        }
    }

    // ——— Rust 函数签名（必须和 jni.rs 精准对齐）———
    // rebleach_start(handle: i64, config_json: *const c_char) -> i64
    // rebleach_stop(handle: i64)
    // rebleach_poll_event(handle: i64, out_buf: *mut c_char, out_cap: i32) -> i32
    // rebleach_push_video_frame(handle: i64, is_key: i8, annex_b_ptr: *const u8, len: i32)
    // rebleach_push_audio_frame(handle: i64, pcm_ptr: *const u8, len: i32)
    // rebleach_approve_connection(handle: i64, allow: i8)
    // rebleach_set_bitrate(handle: i64, kbps: i32)

    /** 启动会话。返回 Rust 侧 handle（0 = 失败）。
     *  configJson 示例：{"role":"agent","server":"wss://xxx","fps":30} */
    external fun start(handle: Long, configJson: ByteArray): Long
    external fun stop(handle: Long)
    external fun pollEvent(handle: Long, outBuf: ByteArray, outCap: Int): Int
    external fun pollFrame(handle: Long, outBuf: ByteArray, outCap: Int, outIsKey: ByteArray): Int
    external fun pushVideoFrame(handle: Long, isKey: Byte, annexB: ByteArray)
    external fun pushAudioFrame(handle: Long, pcm: ByteArray)
    external fun approveConnection(handle: Long, allow: Byte)
    external fun setBitrate(handle: Long, kbps: Int)

    // ——— Kotlin 侧封装：UTF-8 编码 + 事件分发 ———

    private val handleGen = java.util.concurrent.atomic.AtomicLong(1)

    /** 启动会话 + 后台事件轮询。
     *  onEvent 回调在 Main dispatcher 上，直接 update UI 即可。 */
    fun startSession(
        configJson: String,
        onEvent: (String) -> Unit,
        onReady: (Long) -> Unit,
        onError: (String) -> Unit
    ) {
        if (!ensureLoaded()) {
            onError("Rust .so 加载失败: ${loadError}")
            return
        }

        val handle = handleGen.incrementAndGet()
        val bytes = configJson.toByteArray(StandardCharsets.UTF_8)

        val result = try {
            start(handle, bytes)
        } catch (t: UnsatisfiedLinkError) {
            Log.e(TAG, "JNI 函数未找到", t)
            onError("JNI 加载失败：${t.message}")
            return
        } catch (t: Throwable) {
            Log.e(TAG, "start 崩溃", t)
            onError("Rust 崩溃：${t.message}")
            return
        }

        if (result == 0L) {
            onError("Rust 侧 start 返回 0（失败）")
            return
        }

        Log.i(TAG, "会话已启动，handle=$handle")
        onReady(handle)

        // 启动后台轮询
        scope.launch {
            val buf = ByteArray(BUF_SIZE)
            while (isActive) {
                val n = try { pollEvent(result, buf, buf.size) } catch (_: Throwable) { -1 }
                when {
                    n > 0 -> {
                        val json = String(buf, 0, n, StandardCharsets.UTF_8)
                        Log.d(TAG, "事件: $json")
                        withContext(Dispatchers.Main.immediate) { onEvent(json) }
                    }
                    n == 0 -> { /* 队列空，下次再来 */ }
                    else -> {
                        Log.w(TAG, "pollEvent 返回 $n，停止轮询")
                        break
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    fun stopSession(handle: Long) {
        if (ensureLoaded()) {
            try { stop(handle) } catch (_: Throwable) {}
        }
        Log.i(TAG, "会话已停止，handle=$handle")
    }

    /** 被控端把 MediaCodec 编码好的 H.264 Annex-B 帧喂给 Rust。 */
    fun feedVideoFrame(handle: Long, isKey: Boolean, annexB: ByteArray) {
        if (ensureLoaded()) {
            try { pushVideoFrame(handle, if (isKey) 1 else 0, annexB) } catch (_: Throwable) {}
        }
    }

    /** 被控端：16kHz mono i16 PCM。 */
    fun feedAudioFrame(handle: Long, pcm: ByteArray) {
        if (ensureLoaded()) {
            try { pushAudioFrame(handle, pcm) } catch (_: Throwable) {}
        }
    }
}
