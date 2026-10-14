# Margin's picker is only called from Rust (JNI), so release builds must keep it whole.
-keep class {{package}}.MarginPicker {
  native <methods>;
  public static <methods>;
}
