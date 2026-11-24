package app.oj.jdbc;

import java.io.PrintStream;
import java.nio.charset.StandardCharsets;

/** 装载 drivers 目录全部 jar，并把 stdout 重定向到 stderr（防止驱动日志污染 NDJSON 协议流）。 */
final class DriverLoader {
    private static volatile ClassLoader loader;

    private DriverLoader() {}

    /** 返回装载 driversDir 全部 *.jar 的 URLClassLoader，并设为 TCCL。重复调用幂等。 */
    static ClassLoader load(java.nio.file.Path driversDir) throws Exception {
        ClassLoader cached = loader;
        if (cached != null) {
            return cached;
        }
        System.setOut(new PrintStream(System.err, true, StandardCharsets.UTF_8));
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
        // child-first：驱动对 app CL 不可见（生产形态 java -jar 下 app CL 无驱动），
        // 否则 DriverManager.isDriverAllowed 会以调用方可见性为由拒绝驱动。
        java.net.URLClassLoader ucl = new java.net.URLClassLoader(urls, DriverLoader.class.getClassLoader()) {
            @Override
            protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        if (name.startsWith("java.") || name.startsWith("javax.")
                                || name.startsWith("sun.") || name.startsWith("com.sun.")
                                || name.startsWith("jdk.")) {
                            c = getParent().loadClass(name);
                        } else {
                            try {
                                c = findClass(name);
                            } catch (ClassNotFoundException e) {
                                c = getParent().loadClass(name);
                            }
                        }
                    }
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
        };
        Thread.currentThread().setContextClassLoader(ucl);
        registerShims(ucl);
        loader = ucl;
        return ucl;
    }

    /** 把 UCL 可见的驱动（ServiceLoader 扫描 META-INF/services）逐一包 shim 注册。 */
    private static void registerShims(ClassLoader ucl) throws java.sql.SQLException {
        java.util.Set<String> seen = new java.util.HashSet<>();
        for (java.sql.Driver d : java.util.ServiceLoader.load(java.sql.Driver.class, ucl)) {
            if (seen.add(d.getClass().getName())) {
                java.sql.DriverManager.registerDriver(new DriverShim(d));
            }
        }
    }

    /** 已显式装载过的 driver_class（v0.1.147 评审收口：去重——此前每个带逃生舱的
     *  connect 都向 DriverManager 追加一条 DriverInfo + 一个 UCL 驱动实例，长活进程
     *  慢性增长）。 */
    private static final java.util.Set<String> ENSURED =
            java.util.concurrent.ConcurrentHashMap.newKeySet();

    /** 信封可选 driver_class：老驱动缺 META-INF/services 条目时的显式装载逃生舱。 */
    static void ensureDriver(String driverClass) throws Exception {
        if (driverClass == null || driverClass.isEmpty() || !ENSURED.add(driverClass)) {
            return;
        }
        ClassLoader cl = loader != null ? loader : DriverLoader.class.getClassLoader();
        Class<?> cls = Class.forName(driverClass, true, cl);
        // 同样经 shim 注册：直接 newInstance 出来的实例是 UCL 类，DriverManager 一样拒绝
        java.sql.DriverManager.registerDriver(
                new DriverShim((java.sql.Driver) cls.getDeclaredConstructor().newInstance()));
    }
}
