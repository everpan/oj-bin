路由规则改进

一、路由映射
文件路径：

<root>/<module>/<...path>/<feature>/api.ts|js

对应 URL：

<base>/<module>/<...path>/<feature>/

即：以 api.ts 或 api.js 结尾的路由文件，去掉根目录 <root> 和末尾的 api.ts|js 后，映射为对应的基础路由。

二、动态参数规则
对于以 api.ts|js 结尾的路由文件，动态参数必须以完整路径段的形式定义，不能嵌入到某个路径段的中间。例如，允许 /{cc}/，不允许 /a{cc}b/。

为避免 {...} 出现在文件路径中导致 shell 等脚本解析混淆，文件系统中的动态参数段统一使用 _name_ 表示；映射到 URL 时，再转换为 {name}。

三、示例
文件路径：

/v1/api/_aa_/bb/_cc_/dd/ee/ff/api.ts

对应基础路由：

/v1/api/{aa}/bb/{cc}/dd/ee/ff/

其中：

_aa_ 映射为 {aa}

_cc_ 映射为 {cc}

所有类似 _name_ 的路径段均按此规则处理。

四、目的
通过统一使用 _name_ 表示文件路径中的动态参数段，避免文件路径中出现 {aa}、{cc} 等字符，降低 shell 等脚本的解析与转义成本，同时保持 URL 动态参数语义清晰。
