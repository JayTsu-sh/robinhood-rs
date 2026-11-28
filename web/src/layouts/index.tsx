import { ClusterOutlined, DatabaseOutlined, SwapOutlined } from '@ant-design/icons';
import { Link, Outlet, useLocation } from '@umijs/max';
import { ConfigProvider, Layout, Menu } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import { useEffect } from 'react';
import './layout.css';

const navigation = [
  { key: '/policies', title: '分层策略', icon: <SwapOutlined /> },
  { key: '/resources', title: 'Lustre 集群', icon: <ClusterOutlined /> },
];

export default function ConsoleLayout() {
  const { pathname } = useLocation();
  const page = navigation.find(({ key }) => pathname === key || pathname.startsWith(`${key}/`));
  useEffect(() => { document.documentElement.lang = 'zh-CN'; }, []);
  return (
    <ConfigProvider
      locale={zhCN}
      button={{ autoInsertSpace: false }}
      theme={{ token: { colorPrimary: '#2563eb', borderRadius: 8 } }}
    >
      <Layout className="console">
        <Layout.Sider
          className="console-sidebar"
          width={224}
          collapsedWidth={72}
          breakpoint="md"
          trigger={null}
          theme="dark"
        >
          <Link to="/policies" className="console-brand" aria-label="Lustre 控制台">
            <span className="console-mark" aria-hidden="true"><DatabaseOutlined /></span>
            <span className="console-brand-name">Lustre 控制台</span>
          </Link>
          <nav aria-label="控制台导航">
            <Menu
              theme="dark"
              mode="inline"
              selectedKeys={page ? [page.key] : []}
              items={navigation.map(({ key, title, icon }) => ({
                key,
                icon,
                title,
                label: <Link to={key} aria-current={pathname === key ? 'page' : undefined}>{title}</Link>,
              }))}
            />
          </nav>
        </Layout.Sider>
        <Layout className="console-workspace">
          <Layout.Header className="console-header">
            <span>在线冷热分层</span>
            {page && <><span aria-hidden="true">/</span><strong>{page.title}</strong></>}
          </Layout.Header>
          <Layout.Content className="console-content"><Outlet /></Layout.Content>
        </Layout>
      </Layout>
    </ConfigProvider>
  );
}
