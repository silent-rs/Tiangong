import { MainApp } from './pages/MainApp'
import { ToastProvider } from './components/Toast'
import { CentralNotificationBridge } from './components/central/CentralNotificationBridge'
import { StartupPrepareGate } from './components/StartupPrepareGate'
import { useTheme } from './hooks/useTheme'

function App() {
  // 初始化主题（确保 dark class 在页面加载时立即应用）
  useTheme();

  return (
    <ToastProvider>
      <CentralNotificationBridge />
      <StartupPrepareGate>
        <MainApp />
      </StartupPrepareGate>
    </ToastProvider>
  )
}

export default App
