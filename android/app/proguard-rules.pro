# JNI entry points are called by name from the JVM.
-keepclasseswithmembernames class * {
    native <methods>;
}
-keep class dev.quarkdrive.android.QuarkdriveNative { *; }
