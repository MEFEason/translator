package com.translator.app

import android.app.Activity
import android.content.pm.ActivityInfo
import android.content.res.Configuration

/**
 * 屏幕方向策略：**手机锁竖屏，平板可旋转**。
 *
 * ## 为什么要运行时判定
 *
 * Manifest 的 `android:screenOrientation` 是**静态**的，没法按设备类型
 * 区分 —— 写死 `portrait` 平板也被锁死，写 `fullUser` 手机又会横躺。
 *
 * Android 官方的推荐做法（见「Restrict app orientation on phones and
 * foldables」指南）正是：**Manifest 声明宽松值 + 运行时按窗口尺寸收紧**。
 * 本类就是这个策略的唯一实现处。
 *
 * ## 平板判定口径
 *
 * `smallestScreenWidthDp >= 600`（即 sw600dp）—— Android 官方对「平板/
 * 大屏」的标准定义，与资源限定符 `values-sw600dp` 语义一致。
 *
 * 用 `smallestScreenWidthDp` 而不是「当前窗口短边」的原因：
 * 它是**设备属性**，分屏/小窗时不会变（手机分屏仍是手机），
 * 而折叠屏展开时会从 ~320 跳到 ~673 → 自动从「手机策略」切成「平板策略」。
 *
 * ## 折叠屏 / 多窗口
 *
 * 方向变化不重建 Activity（Manifest 的 `configChanges` 已含
 * `orientation|screenSize|smallestScreenSize`），所以每次配置变化都要
 * **重新判定**一次 —— 折叠屏展开时立刻从锁竖屏切成可旋转。
 */
object OrientationPolicy {

    /** Android 官方的大屏阈值（sw600dp）。 */
    private const val TABLET_MIN_SW_DP = 600

    /**
     * 当前配置下是否按平板处理。
     */
    fun isTablet(config: Configuration): Boolean =
        config.smallestScreenWidthDp >= TABLET_MIN_SW_DP

    /**
     * 把方向策略应用到 Activity。
     *
     * - 平板 → `SCREEN_ORIENTATION_FULL_USER`：允许四个方向（含 180°
     *   翻转），但**尊重用户的自动旋转开关** —— 用户锁了旋转就不转，
     *   这与系统其它应用的行为一致。
     * - 手机 → `SCREEN_ORIENTATION_PORTRAIT`：锁定正竖屏。
     *   字幕要贴着屏幕底部看，横屏没法用，故手机端不做横屏。
     */
    fun apply(activity: Activity) {
        val want = if (isTablet(activity.resources.configuration)) {
            ActivityInfo.SCREEN_ORIENTATION_FULL_USER
        } else {
            ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
        }
        // ⚠️ 只在真的需要改时才设：setRequestedOrientation 会触发一次
        //    configuration change，无条件设置 → onConfigurationChanged
        //    → 再设 → 无限循环。
        if (activity.requestedOrientation != want) {
            activity.requestedOrientation = want
        }
    }

    /**
     * 供 Dart 侧查询的设备画像。
     *
     * 判定以 **Kotlin 为唯一权威**（Dart 侧拿不到 `smallestScreenWidthDp`），
     * 避免 Dart 用「窗口短边」猜出与 Android 不一致的结果
     * （例如手机分屏时窗口变宽被误判成平板 → 导航栏形态突变）。
     *
     * ## 性能字段（2026-10-28 任务 #23：Android 线程调整器）
     *
     * Windows 端的档位是「用户手选 + 桌面占用基准」，Android 没有那套
     * 基准数据，改用**硬件画像**推导默认线程数：
     * - `cores`：Runtime 可见核数（big.LITTLE 下常只报大核或全部，
     *   与 /system/devices 口径有差异，但足以分档）
     * - `ramMb`：应用可用堆外的物理内存近似（ActivityManager.memoryClass
     *   不可靠，用 totalMem）
     * - `lowRam`：系统 lowMemoryDevice 标记（Go 版设备 true）
     * 产品定位（MEMORY：会议字幕跟手是核心价值）→ **默认满档**，
     * 只有 lowRam 或核数极少时才降档。
     */
    fun profileOf(activity: Activity): Map<String, Any> {
        val config = activity.resources.configuration
        val am = activity.getSystemService(android.content.Context.ACTIVITY_SERVICE)
            as android.app.ActivityManager
        val memInfo = android.app.ActivityManager.MemoryInfo().also { am.getMemoryInfo(it) }
        val cores = Runtime.getRuntime().availableProcessors()
        return mapOf(
            "isTablet" to isTablet(config),
            "smallestWidthDp" to config.smallestScreenWidthDp,
            "cores" to cores,
            "ramMb" to (memInfo.totalMem / 1024 / 1024),
            "lowRam" to am.isLowRamDevice,
        )
    }
}
