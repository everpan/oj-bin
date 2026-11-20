package app.oj.jdbc;

import java.io.PrintStream;
import java.nio.charset.StandardCharsets;

/** 装载 drivers 目录全部 jar，并把 stdout 重定向到 stderr（防止驱动日志污染 NDJSON 协议流）。 */
final class DriverLoader {
    private static volatile ClassLoader loader;

    private DriverLoader() {}

    /** 返回装载 driversDir 全部 *.jar 的 URLClassLoader，并设为 TCCL。重复调用幂等。 */
    static ClassLoader load(java.nio.file.Path driversDir) throws Exception {
        System.setOut(new PrintStream(System.err, true, StandardCharsets.UTF_8));
        ClassLoader cached = loader;
        if (cached != null) {
            return cached;
        }
        java.util.List<java.nio.file.Path> jars;
        try (java.util.stream.Stream<java.nio.file.Path> s = java.nio.file.Files.list(driversDir)) {
            jars = s.filter(p -> p.getFileName().toString().endsWith(".jar"))
                    .sorted()
                    .collect(java.util.stream.Collectors.toList());
        }
        java.net.URL[] urls = new java.net.URL[jars.size()];
        for (int i = 0; i < jars.size(); i++) {
            urls[i] = jars.get(i).toUri().toURL();
        }
        java.net.URLClassLoader ucl =
                new java.net.URLClassLoader(urls, DriverLoader.class.getClassLoader());
        Thread.currentThread().setContextClassLoader(ucl);
        loader = ucl;
        return ucl;
    }

    /** 信封可选 driver_class：老驱动缺 META-INF/services 条目时的显式装载逃生舱。 */
    static void ensureDriver(String driverClass) throws Exception {
        if (driverClass == null || driverClass.isEmpty()) {
            return;
        }
        ClassLoader cl = loader != null ? loader : DriverLoader.class.getClassLoader();
        Class.forName(driverClass, true, cl);
    }
}
