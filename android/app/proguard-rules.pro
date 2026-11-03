# Rebleach Android ProGuard 规则
# Rust .so 里的符号不能被 strip

# Rust 所有 extern "system" 函数都是 #[no_mangle] 的，
# 理论上 ProGuard 不会动 .so 里的 native symbols，
# 但 keep 一下类名以防混淆后 System.loadLibrary 失败

-keep class com.rebleach.jni.NativeBridge { *; }
-keepclasseswithmembernames class * { native <methods>; }
-keep class * extends android.app.Service { *; }
