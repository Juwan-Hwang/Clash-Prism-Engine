/**
 * smart-grouping — 智能分组插件入口
 *
 * @prism-scope: subscribe
 * @prism-permissions: store
 */

function main(ctx) {
    const { proxies, groups, log } = ctx.utils;

    // 1. 过滤无效节点（无 server 或端口非法）
    const beforeCount = proxies.count();
    proxies.remove(p => !p.server || p.port <= 0 || p.port > 65535);
    const remaining = proxies.count();
    log.info(`过滤后剩余 ${remaining} 个节点（移除 ${beforeCount - remaining} 个）`);

    // 2. 重命名：添加国旗前缀
    proxies.rename(/^港/, "🇭🇰 香港");
    proxies.rename(/^日/, "🇯🇵 日本");
    proxies.rename(/^美/, "🇺🇸 美国");
    proxies.rename(/^新/, "🇸🇬 新加坡");
    proxies.rename(/^英/, "🇬🇧 英国");
    proxies.rename(/^德/, "🇩🇪 德国");
    proxies.rename(/^韩/, "🇰🇷 韩国");

    // 3. 按国旗分组
    const regions = proxies.groupBy(/^(🇭🇰|🇯🇵|🇺🇸|🇸🇬|🇬🇧|🇩🇪|🇰🇷)/);

    for (const [region, nodes] of regions) {
        if (nodes.length === 0) continue;

        const groupName = `${region} Auto`;

        // 创建 url-test 自动选择组
        groups.create({
            name: groupName,
            type: "url-test",
            proxies: nodes.map(p => p.name),
            url: "http://www.gstatic.com/generate_204",
            interval: 300,
            tolerance: 50,
        });

        // 将新组添加到 PROXY 主组
        groups.addProxy("PROXY", groupName);

        log.info(`✅ 创建分组: ${groupName} (${nodes.length} 个节点)`);
    }

    log.info("智能分组完成 ✨");
}
