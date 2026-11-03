package com.rebleach.agent

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.DisplayMetrics
import android.util.Log
import com.rebleach.jni.NativeBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/** 被控端前台服务：持有 MediaProjection + VirtualDisplay + MediaCodec。
 *
 * 启动流程：
 *   1. MainActivity 调 ActivityResultLauncher 拿 MediaProjection resultCode + data
 *   2. 用 startForegroundService 启动本服务，把 resultCode/data 放进 Intent
 *   3. createMediaProjection(resultCode, data) → VirtualDisplay → MediaCodec → 编码回调里 feedVideoFrame
 *
 * 停止流程：
 *   stopSelf() → release codec + release projection → stopForeground(true)
 */
class AgentForegroundService : Service() {

    private val tag = "AgentForeground"
    private val CHANNEL_ID = "rebleach-agent"

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var encodeJob: Job? = null
    private var sessionHandle: Long = 0

    // 编码参数（M0 默认值，后续按 MediaCodecInfo capabilities 调整）
    private val VIDEO_WIDTH = 1280
    private val VIDEO_HEIGHT = 720
    private val VIDEO_BITRATE = 4_000_000 // 4 Mbps
    private val VIDEO_FPS = 30
    private val VIDEO_IF_INTERVAL_SEC = 2

    companion object {
        const val ACTION_START = "com.rebleach.agent.START"
        const val EXTRA_RESULT_CODE = "result_code"
        const val EXTRA_DATA = "data"
        const val EXTRA_SERVER = "server"

        /** 快捷启动（带 MediaProjection 授权结果） */
        fun start(context: Context, resultCode: Int, data: Intent?, server: String) {
            val intent = Intent(context, AgentForegroundService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_RESULT_CODE, resultCode)
                putExtra(EXTRA_DATA, data)
                putExtra(EXTRA_SERVER, server)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        startForeground(1001, buildNotification("Rebleach 被控端运行中"))
        Log.i(tag, "AgentForegroundService.onCreate")
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_START) {
            val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, Activity.RESULT_CANCELED)
            val data = intent.getParcelableExtra<Intent>(EXTRA_DATA)
            val server = intent.getStringExtra(EXTRA_SERVER) ?: ""

            Log.i(tag, "收到 START 意图，resultCode=$resultCode, server=$server")

            // 先启动 Rust 会话（M0 stub：start 后返回 handle，poll 到 ready 事件）
            val configJson = org.json.JSONObject()
                .put("role", "agent")
                .put("server", server)
                .put("id", com.rebleach.deviceId(this))
                .put("key_file", com.rebleach.keyFilePath(this))
                .put("fps", VIDEO_FPS)
                .toString()
            NativeBridge.startSession(
                configJson = configJson,
                onEvent = { json -> Log.i(tag, "Rust event: $json") },
                onReady = { handle ->
                    sessionHandle = handle
                    Log.i(tag, "Rust 会话就绪 handle=$handle，开始投屏")
                    startProjectionAndCodecs(resultCode, data)
                },
                onError = { err ->
                    Log.e(tag, "Rust 启动失败: $err")
                    stopSelf()
                }
            )
        }
        return START_STICKY
    }

    private fun startProjectionAndCodecs(resultCode: Int, data: Intent?) {
        if (resultCode != Activity.RESULT_OK || data == null) {
            Log.w(tag, "MediaProjection 授权无效：resultCode=$resultCode")
            return
        }
        val mpm = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        projection = mpm.getMediaProjection(resultCode, data)
        projection?.let { proj ->
            proj.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() {
                    Log.w(tag, "MediaProjection 被系统停止，清理资源")
                    cleanup()
                }
            }, null)
            createVirtualDisplay(proj)
            startEncoder()
        }
    }

    private fun createVirtualDisplay(proj: MediaProjection) {
        val metrics = resources.displayMetrics

        // 按屏幕实际分辨率创建；大分辨率设备上可以缩到 720p 省带宽
        val w = metrics.widthPixels
        val h = metrics.heightPixels
        Log.i(tag, "VirtualDisplay 分辨率 ${w}x${h}")

        virtualDisplay = proj.createVirtualDisplay(
            "RebleachAgent",
            w, h, metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            null, null, null
        )
    }

    private fun startEncoder() {
        val mimeType = "video/avc"

        // 创建 H.264 硬编码器
        val mc = try {
            MediaCodec.createEncoderByType(mimeType)
        } catch (e: IllegalArgumentException) {
            Log.e(tag, "设备不支持 H.264 编码: ${e.message}"); return
        }

        val encoderName = mc.codecInfo?.name ?: "unknown"

        val format = MediaFormat.createVideoFormat(mimeType, VIDEO_WIDTH, VIDEO_HEIGHT).apply {
            setInteger(MediaFormat.KEY_BIT_RATE, VIDEO_BITRATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, VIDEO_FPS)
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface
            )
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, VIDEO_IF_INTERVAL_SEC)
        }

        mc.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val inputSurface = mc.createInputSurface()

        // VirtualDisplay 输出到编码器 Surface
        val vd = virtualDisplay
        if (vd == null) { Log.e(tag, "VirtualDisplay 未就绪"); return }

        // 重新创建 VirtualDisplay 让它输出到编码器 Surface
        vd.release()
        val dm = getSystemService(DISPLAY_SERVICE) as DisplayManager
        virtualDisplay = projection?.createVirtualDisplay(
            "RebleachAgent",
            VIDEO_WIDTH, VIDEO_HEIGHT, 1,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            inputSurface, null, null
        )

        mc.start()
        codec = mc

        Log.i(tag, "编码器已启动：${encoderName}, ${VIDEO_WIDTH}x${VIDEO_HEIGHT}@${VIDEO_FPS}fps")

        // 启动编码输出循环
        encodeJob = scope.launch { drainEncoder(mc) }
    }

    /** 持续从编码器 output buffer 拿 H.264 Annex-B 帧，喂给 Rust。 */
    private suspend fun drainEncoder(mc: MediaCodec) {
        val bufferInfo = MediaCodec.BufferInfo()
        while (true) {
            val idx = mc.dequeueOutputBuffer(bufferInfo, 10_000)
            if (idx >= 0) {
                val out = mc.getOutputBuffer(idx) ?: continue
                // MediaCodec 输出已经是 Annex-B 格式（含 4-byte start code）
                val data = ByteArray(bufferInfo.size)
                out.position(bufferInfo.offset)
                out.limit(bufferInfo.offset + bufferInfo.size)
                out.get(data)

                val isKey = (bufferInfo.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                NativeBridge.feedVideoFrame(sessionHandle, isKey, data)

                mc.releaseOutputBuffer(idx, false)
            } else if (idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                Log.i(tag, "编码器格式变更: ${mc.outputFormat}")
            }
            // 负值（TIMEOUT / TRY_AGAIN_LATER）循环继续
        }
    }

    private fun cleanup() {
        encodeJob?.cancel(); encodeJob = null
        codec?.stop(); codec?.release(); codec = null
        virtualDisplay?.release(); virtualDisplay = null
        projection?.stop(); projection = null
        NativeBridge.stopSession(sessionHandle)
        sessionHandle = 0
        Log.i(tag, "资源已释放")
    }

    override fun onDestroy() {
        cleanup()
        super.onDestroy()
    }

    // ——— 通知 ———

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val ch = NotificationChannel(
                CHANNEL_ID, "Rebleach 被控端",
                NotificationManager.IMPORTANCE_LOW
            ).apply { setShowBadge(false) }
            nm.createNotificationChannel(ch)
        }
    }

    private fun buildNotification(text: String): Notification {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Rebleach")
                .setContentText(text)
                .setSmallIcon(android.R.drawable.ic_media_play)
                .setOngoing(true).build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("Rebleach")
                .setContentText(text)
                .setSmallIcon(android.R.drawable.ic_media_play)
                .setOngoing(true).build()
        }
    }
}
