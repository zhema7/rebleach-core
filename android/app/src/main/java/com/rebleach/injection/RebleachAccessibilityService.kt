package com.rebleach.injection

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.util.Log
import android.view.accessibility.AccessibilityEvent

/** 被控端无障碍注入（默认零配置可用）。
 *  dispatchGesture 能做：点击、滑动、长按、滚动、双指 pinch（Android 11+）。
 *
 *  精度限制：没有鼠标悬停、右键、滚轮。主控端需要在本地叠一个光标层
 *  给用户视觉反馈。
 *
 *  提升方案：检测到 Shizuku 时升级为真实鼠标 + 完整按键。 */
class RebleachAccessibilityService : AccessibilityService() {

    private val tag = "RebleachA11y"

    override fun onAccessibilityEvent(event: AccessibilityEvent?) { /* 主控端发起手势，这里不需要监听 */ }
    override fun onInterrupt() { Log.w(tag, "无障碍服务中断") }
    override fun onServiceConnected() { super.onServiceConnected(); Log.i(tag, "Rebleach 无障碍服务就绪") }

    /** 点按：dispatchGesture 一次点击。 */
    fun click(x: Float, y: Float) {
        val path = Path().apply { moveTo(x, y); lineTo(x, y) }
        val stroke = GestureDescription.StrokeDescription(path, 50, 50)
        val builder = GestureDescription.Builder().addStroke(stroke)
        dispatchGesture(builder.build(), null, null)
    }

    /** 滑动。 */
    fun swipe(fromX: Float, fromY: Float, toX: Float, toY: Float, durationMs: Long = 400) {
        val path = Path().apply { moveTo(fromX, fromY); lineTo(toX, toY) }
        val stroke = GestureDescription.StrokeDescription(path, 50, durationMs)
        val builder = GestureDescription.Builder().addStroke(stroke)
        dispatchGesture(builder.build(), null, null)
    }
}
