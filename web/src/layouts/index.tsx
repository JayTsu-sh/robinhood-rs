import { ClusterOutlined, SwapOutlined } from '@ant-design/icons';
import { Link, Outlet, useLocation } from '@umijs/max';
import { ConfigProvider, Layout, Menu } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import { useEffect } from 'react';
import './layout.css';

const navigation = [
  {
    key: 'storage', title: '存储资源', icon: <ClusterOutlined aria-hidden="true" />,
    items: [{ key: '/resources', title: 'Lustre 集群' }],
  },
  {
    key: 'tiering', title: '数据分层', icon: <SwapOutlined aria-hidden="true" />,
    items: [{ key: '/policies', title: '分层策略' }, { key: '/runs', title: '执行记录' }],
  },
];

const pages = navigation.flatMap(({ title: section, items }) => items.map((item) => ({ ...item, section })));

export default function ConsoleLayout() {
  const { pathname } = useLocation();
  const page = pages.find(({ key }) => pathname === key || pathname.startsWith(`${key}/`));
  useEffect(() => { document.documentElement.lang = 'zh-CN'; }, []);
  return (
    <ConfigProvider
      locale={zhCN}
      button={{ autoInsertSpace: false }}
      theme={{
        token: { colorPrimary: '#2563eb', borderRadius: 8 },
        components: { Menu: { darkSubMenuItemBg: '#102136' } },
      }}
    >
      <Layout className="console">
        <Layout.Sider
          className="console-sidebar"
          width={224}
          theme="dark"
        >
          <Link to="/policies" className="console-brand" aria-label="Robinhood">
            <span className="console-brand-name" aria-hidden="true">Robinhood</span>
          </Link>
          <nav aria-label="控制台导航">
            <Menu
              theme="dark"
              mode="inline"
              defaultOpenKeys={navigation.map(({ key }) => key)}
              selectedKeys={page ? [page.key] : []}
              items={navigation.map(({ key, title, icon, items }) => ({
                key,
                icon,
                label: title,
                children: items.map(({ key: path, title: label }) => ({
                  key: path,
                  label: <Link to={path} aria-current={pathname === path ? 'page' : undefined}>{label}</Link>,
                })),
              }))}
            />
          </nav>
        </Layout.Sider>
        <Layout className="console-workspace">
          <Layout.Header className="console-header">
            {page && <><span>{page.section}</span><span aria-hidden="true">/</span><strong>{page.title}</strong></>}
          </Layout.Header>
          <Layout.Content className="console-content">
            <nav className="console-mobile-nav" aria-label="控制台导航">
              {pages.map(({ key, title }) => (
                <Link key={key} to={key} aria-current={page?.key === key ? 'page' : undefined}>{title}</Link>
              ))}
            </nav>
            <Outlet />
          </Layout.Content>
        </Layout>
      </Layout>
    </ConfigProvider>
  );
}
