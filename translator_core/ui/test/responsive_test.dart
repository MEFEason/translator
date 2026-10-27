import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:translator/services/responsive.dart';

/// 屏幕适配测试。
///
/// 覆盖三种形态：
/// - 手机竖屏（411×891）→ 底部导航栏、正文不限宽
/// - 平板竖屏（800×1280）→ 侧边导航栏、正文限宽 840
/// - 平板横屏（1280×800）→ 侧边导航栏、正文限宽 960
///
/// ## 为什么用 widget 测试而非纯逻辑测试
///
/// `ResponsiveBody` 的关键正确性在于 **ListView 在限宽容器里仍拿到有界
/// 高度**。若约束传递写错（比如用 `Center` 放松成无界），运行时会抛
/// "Vertical viewport was given unbounded height" —— 只有真正 layout
/// 一次才能发现，纯逻辑断言测不出来。
void main() {
  group('Responsive 尺寸判定', () {
    testWidgets('平板竖屏：sw=800 判定为平板，限宽 840', (tester) async {
      const profile = DeviceProfile(isTablet: true, smallestWidthDp: 800);
      late BuildContext ctx;
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(size: Size(800, 1280)),
          child: DeviceProfileScope(
            profile: profile,
            child: Builder(builder: (c) {
              ctx = c;
              return const SizedBox.shrink();
            }),
          ),
        ),
      );
      expect(Responsive.isTablet(ctx), isTrue);
      expect(
          Responsive.contentMaxWidth(ctx), Responsive.tabletMaxWidthPortrait);
      expect(Responsive.useSideNavigation(ctx), isTrue);
    });

    testWidgets('平板横屏：限宽放宽到 960', (tester) async {
      late BuildContext ctx;
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(size: Size(1280, 800)),
          child: DeviceProfileScope(
            profile: const DeviceProfile(isTablet: true, smallestWidthDp: 800),
            child: Builder(builder: (c) {
              ctx = c;
              return const SizedBox.shrink();
            }),
          ),
        ),
      );
      expect(
          Responsive.contentMaxWidth(ctx), Responsive.tabletMaxWidthLandscape);
      expect(Responsive.isLandscape(ctx), isTrue);
    });

    testWidgets('手机：非平板，正文不限宽，用底栏', (tester) async {
      late BuildContext ctx;
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(size: Size(411, 891)),
          child: DeviceProfileScope(
            profile: const DeviceProfile(isTablet: false, smallestWidthDp: 411),
            child: Builder(builder: (c) {
              ctx = c;
              return const SizedBox.shrink();
            }),
          ),
        ),
      );
      expect(Responsive.isTablet(ctx), isFalse);
      expect(Responsive.contentMaxWidth(ctx), double.infinity);
      expect(Responsive.useSideNavigation(ctx), isFalse);
    });

    testWidgets('折叠屏展开（sw 320→673）判定从手机切成平板', (tester) async {
      late BuildContext foldedCtx;
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(size: Size(320, 700)),
          child: DeviceProfileScope(
            profile: const DeviceProfile(isTablet: false, smallestWidthDp: 320),
            child: Builder(builder: (c) {
              foldedCtx = c;
              return const SizedBox.shrink();
            }),
          ),
        ),
      );
      expect(Responsive.isTablet(foldedCtx), isFalse);

      late BuildContext unfoldedCtx;
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(size: Size(673, 841)),
          child: DeviceProfileScope(
            profile: const DeviceProfile(isTablet: true, smallestWidthDp: 673),
            child: Builder(builder: (c) {
              unfoldedCtx = c;
              return const SizedBox.shrink();
            }),
          ),
        ),
      );
      expect(Responsive.isTablet(unfoldedCtx), isTrue);
    });
  });

  group('ResponsiveBody 布局约束', () {
    testWidgets('平板横屏：ListView 被限宽到 960 且水平居中', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: MediaQuery(
              data: const MediaQueryData(size: Size(1280, 800)),
              child: DeviceProfileScope(
                profile:
                    const DeviceProfile(isTablet: true, smallestWidthDp: 800),
                child: ResponsiveBody(
                  child: ListView(
                    children: const [
                      SizedBox(height: 2000, child: Text('内容')),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ),
      );

      // 关键：ListView 必须能布局（有界高度）。
      // 若约束传递写错，这里会抛 unbounded height 异常。
      expect(tester.takeException(), isNull);

      final listSize = tester.getSize(find.byType(ListView));
      expect(listSize.width, Responsive.tabletMaxWidthLandscape);
      expect(listSize.height, 800);

      // 居中：左侧留白 = (1280 - 960) / 2
      final listLeft = tester.getTopLeft(find.byType(ListView)).dx;
      expect(listLeft, closeTo((1280 - 960) / 2, 0.5));
    });

    testWidgets('手机：ListView 铺满全宽（不限宽）', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: MediaQuery(
              data: const MediaQueryData(size: Size(411, 891)),
              child: DeviceProfileScope(
                profile:
                    const DeviceProfile(isTablet: false, smallestWidthDp: 411),
                child: ResponsiveBody(
                  child: ListView(
                    children: const [SizedBox(height: 500, child: Text('内容'))],
                  ),
                ),
              ),
            ),
          ),
        ),
      );

      expect(tester.takeException(), isNull);
      expect(tester.getSize(find.byType(ListView)).width, 411);
      expect(tester.getTopLeft(find.byType(ListView)).dx, 0);
    });

    testWidgets('平板竖屏：限宽 840', (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: MediaQuery(
              data: const MediaQueryData(size: Size(800, 1280)),
              child: DeviceProfileScope(
                profile:
                    const DeviceProfile(isTablet: true, smallestWidthDp: 800),
                child: ResponsiveBody(
                  child: ListView(
                    children: const [SizedBox(height: 300, child: Text('内容'))],
                  ),
                ),
              ),
            ),
          ),
        ),
      );

      expect(tester.takeException(), isNull);
      expect(tester.getSize(find.byType(ListView)).width,
          Responsive.tabletMaxWidthPortrait);
    });
  });
}
