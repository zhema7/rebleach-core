package com.rebleach

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import com.rebleach.agent.AgentForegroundService
import com.rebleach.jni.NativeBridge
import org.json.JSONObject

/** Rebleach Android 主界面（agent + controller 两 flavor 共用）。
 *
 * 启动时自动 detectBuildFlavor：
 *   packageName 含 ".agent"   → AgentScreen：MediaProjection 授权 → 被控前台服务 → 投屏
 *   packageName 含 ".controller" → ControllerScreen：连接服务器 → 主控被控端 → 解码上屏
 *
 * Rust 核心在 NativeBridge init 时加载；poll event loop 在 startSession 后自动启动。 */
class MainActivity : ComponentActivity() {

    private lateinit var projectionLauncher: ActivityResultLauncher<Intent>

    override fun onCreate(savedInstanceState: Bundle?) {
        // 全局防闪退：捕获任何未处理异常，写 log 而不是直接崩
        val defaultHandler = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            Log.e("RebleachCrash", "Uncaught in thread ${t.name}", e)
            e.printStackTrace()
            defaultHandler?.uncaughtException(t, e)
        }

        super.onCreate(savedInstanceState)

        projectionLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            val code = result.resultCode
            val data = result.data
            Log.i("MainActivity", "MediaProjection 授权结果 code=$code")

            // 必须拿到权限后才能启动前台服务
            requestNotificationPermissionIfNeeded {
                val server = "wss://rebleach.example.com:443" // TODO: 用户配置输入
                AgentForegroundService.start(this, code, data, server)
                Toast.makeText(this, "被控端已启动，开始投屏", Toast.LENGTH_SHORT).show()
            }
        }

        try {
            setContent {
                MaterialTheme {
                    Surface(modifier = Modifier.fillMaxSize()) {
                        val flavor = detectFlavor()
                        if (flavor == "agent") AgentScreen(onRequestProjection = ::requestProjection)
                        else ControllerScreen()
                    }
                }
            }
        } catch (t: Throwable) {
            Log.e("RebleachUI", "setContent 崩溃", t)
            Toast.makeText(this, "界面加载失败: ${t.message}", Toast.LENGTH_LONG).show()
        }
    }

    private fun detectFlavor(): String {
        val pkg = packageName
        return when {
            pkg.endsWith(".agent") -> "agent"
            pkg.endsWith(".controller") -> "controller"
            else -> "controller" // default fallback
        }
    }

    private fun requestProjection() {
        val mpm = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        projectionLauncher.launch(mpm.createScreenCaptureIntent())
    }

    private fun requestNotificationPermissionIfNeeded(after: () -> Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED
            ) {
                registerForActivityResult(
                    ActivityResultContracts.RequestPermission()
                ) { after() }.launch(Manifest.permission.POST_NOTIFICATIONS)
                return
            }
        }
        after()
    }
}

// —— 设备身份配置（服务端用数字 ID 做授权，Rust 需要可写目录存 ed25519 私钥）——

/** 首次启动生成并持久化 9 位数字设备 ID；服务器端授权按这个 ID 建立。 */
fun deviceId(ctx: Context): String {
    val prefs = ctx.getSharedPreferences("rebleach", Context.MODE_PRIVATE)
    prefs.getString("device_id", null)?.let { return it }
    val id = (100_000_000..999_999_999).random().toString()
    prefs.edit().putString("device_id", id).apply()
    return id
}

/** Android 进程 cwd 是 "/"，默认身份路径不可写，必须放到应用私有目录。 */
fun keyFilePath(ctx: Context): String =
    java.io.File(ctx.filesDir, "rebleach/identity.json").absolutePath

/** 写进 config JSON 的公共字段 */
fun buildBaseConfig(ctx: Context, server: String): JSONObject = JSONObject()
    .put("server", server)
    .put("id", deviceId(ctx))
    .put("key_file", keyFilePath(ctx))

// —— 被控端界面 ——

@Composable
fun AgentScreen(onRequestProjection: () -> Unit) {
    var events by remember { mutableStateOf<List<String>>(emptyList()) }
    var serverText by remember { mutableStateOf("wss://47.79.22.180/ws") }
    val ctx = LocalContext.current

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp)
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Text("Rebleach 被控端", fontSize = 24.sp, fontWeight = FontWeight.Bold)

        OutlinedTextField(
            value = serverText,
            onValueChange = { serverText = it },
            label = { Text("信令服务器 (wss://)") },
            modifier = Modifier.fillMaxWidth()
        )

        Button(
            onClick = {
                // 先启动 Rust 会话看能否连上信令（M0: Rust 只 stub ready 事件，没有实际连信令）
                val config = buildBaseConfig(ctx, serverText)
                    .put("role", "agent")
                    .put("fps", 30)
                    .toString()
                NativeBridge.startSession(
                    configJson = config,
                    onEvent = { ev ->
                        events = listOf(ev) + events.take(19) // 留最后 20 条
                        Log.d("Agent", "event=$ev")
                    },
                    onReady = { h ->
                        Log.i("Agent", "Rust 会话 handle=$h 就绪")
                        onRequestProjection() // 请求 MediaProjection
                    },
                    onError = { err ->
                        Toast.makeText(ctx, "启动失败: $err", Toast.LENGTH_LONG).show()
                    }
                )
            },
            modifier = Modifier.fillMaxWidth(),
            contentPadding = PaddingValues(vertical = 12.dp)
        ) { Text("开始投屏") }

        Text("事件日志", fontSize = 16.sp, fontWeight = FontWeight.SemiBold, modifier = Modifier.padding(top = 8.dp))
        Surface(modifier = Modifier.fillMaxWidth(), color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.5f)) {
            Column(modifier = Modifier.padding(8.dp)) {
                if (events.isEmpty()) Text("等待事件...", color = MaterialTheme.colorScheme.onSurfaceVariant)
                events.forEach { Text(it, fontSize = 12.sp, modifier = Modifier.padding(vertical = 2.dp)) }
            }
        }

        Spacer(Modifier.height(16.dp))
        Text("注意：MediaProjection 授权一旦取消，投屏自动停止；需重新点击开始投屏。",
             fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

// —— 主控端界面 ——

@Composable
fun ControllerScreen() {
    var status by remember { mutableStateOf("准备就绪") }
    var serverText by remember { mutableStateOf("wss://47.79.22.180/ws") }
    var peerIdText by remember { mutableStateOf("") }
    var events by remember { mutableStateOf<List<String>>(emptyList()) }
    var handle by remember { mutableStateOf(0L) }
    var connected by remember { mutableStateOf(false) }
    var videoFps by remember { mutableStateOf(0f) }
    var videoKbps by remember { mutableStateOf(0f) }
    var totalFrames by remember { mutableStateOf(0L) }
    val ctx = LocalContext.current

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp)
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Text("Rebleach 主控端", fontSize = 24.sp, fontWeight = FontWeight.Bold)
        Text(
            "我的设备 ID: ${deviceId(ctx)}（服务器端需为该 ID 授予访问被控端的权限）",
            fontSize = 12.sp,
            color = androidx.compose.ui.graphics.Color.Gray
        )

        OutlinedTextField(
            value = serverText, onValueChange = { serverText = it },
            label = { Text("信令服务器 (wss://)") }, modifier = Modifier.fillMaxWidth()
        )
        OutlinedTextField(
            value = peerIdText, onValueChange = { peerIdText = it },
            label = { Text("被控端 ID（可选，空则扫描）") }, modifier = Modifier.fillMaxWidth()
        )

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = {
                    val config = buildBaseConfig(ctx, serverText)
                        .put("role", "controller")
                        .put("peer", peerIdText.trim())
                        .toString()
                    NativeBridge.startSession(
                        configJson = config,
                        onEvent = { ev ->
                            events = listOf(ev) + events.take(19)
                            when {
                                ev.contains("\"type\":\"connecting\"") -> status = "信令连接中..."
                                ev.contains("\"type\":\"error\"") -> { status = "❌ ${JSONObject(ev).optString("msg")}"; connected = false }
                                ev.contains("\"type\":\"closed\"") -> { status = "会话已结束"; connected = false }
                            }
                        },
                        onReady = { handle = it; connected = true; status = "✅ 会话启动 handle=$it，等待视频帧..." },
                        onError = { err -> Toast.makeText(ctx, "启动失败: $err", Toast.LENGTH_LONG).show() }
                    )
                },
                modifier = Modifier.weight(1f),
                contentPadding = PaddingValues(vertical = 12.dp),
                enabled = !connected
            ) { Text("连接") }

            OutlinedButton(
                onClick = {
                    if (handle != 0L) {
                        NativeBridge.stopSession(handle)
                        connected = false; handle = 0L; status = "已断开"
                    }
                },
                modifier = Modifier.weight(1f),
                enabled = connected
            ) { Text("断开") }
        }

        Text(status, modifier = Modifier.align(Alignment.CenterHorizontally))

        // —— 视频预览区 ——
        if (handle != 0L) {
            Card(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 200.dp, max = 400.dp),
                colors = CardDefaults.cardColors(containerColor = androidx.compose.ui.graphics.Color.Black)
            ) {
                Box(modifier = Modifier.fillMaxSize()) {
                    if (connected) {
                        com.rebleach.jni.MediaCodecSurface(
                            handle = handle,
                            isRunning = connected,
                            modifier = Modifier.fillMaxSize(),
                            onStatsUpdate = { fps, kbps, frames ->
                                videoFps = fps; videoKbps = kbps; totalFrames = frames
                            }
                        )
                    }
                    // 右下角叠加统计
                    Column(
                        modifier = Modifier
                            .align(Alignment.BottomEnd)
                            .padding(8.dp)
                            .background(androidx.compose.ui.graphics.Color(0x80000000))
                            .padding(4.dp),
                        horizontalAlignment = Alignment.End
                    ) {
                        Text(
                            if (connected) "🎬 ${"%.0f".format(videoFps)} fps · ${"%.0f".format(videoKbps)} kbps"
                            else "⏳ 等待被控端帧...",
                            fontSize = 12.sp,
                            color = androidx.compose.ui.graphics.Color.White
                        )
                    }
                }
            }
        }

        // —— 事件日志 ——
        Text("事件日志", fontSize = 16.sp, fontWeight = FontWeight.SemiBold, modifier = Modifier.padding(top = 8.dp))
        Surface(
            modifier = Modifier.fillMaxWidth().heightIn(min = 80.dp, max = 200.dp),
            color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.5f)
        ) {
            Column(modifier = Modifier.padding(8.dp).verticalScroll(rememberScrollState())) {
                if (events.isEmpty()) Text("等待事件...", color = MaterialTheme.colorScheme.onSurfaceVariant)
                events.forEach { Text(it, fontSize = 12.sp, modifier = Modifier.padding(vertical = 2.dp)) }
            }
        }
    }
}
