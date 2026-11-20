package app.oj.jdbc;

/** spec §5.4：单 cell 超 16MB 上限的结构化错误；消息含 "cell_too_large" 标记供宿主识别。 */
final class CellTooLargeException extends Exception {
    CellTooLargeException(String column) {
        super("cell_too_large: column '" + column + "' exceeds 16MB limit");
    }
}
