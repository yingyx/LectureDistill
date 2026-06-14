import { useState } from 'react';
import { NavLink, Outlet, useLocation } from 'react-router-dom';
import {
  AppBar,
  Toolbar,
  Typography,
  Drawer,
  List,
  ListItemButton,
  ListItemIcon,
  ListItemText,
  IconButton,
  Box,
  useMediaQuery,
  useTheme,
} from '@mui/material';
import MenuIcon from '@mui/icons-material/Menu';
import DashboardIcon from '@mui/icons-material/Dashboard';
import SourceIcon from '@mui/icons-material/Source';
import OutputIcon from '@mui/icons-material/Output';
import WorkIcon from '@mui/icons-material/Work';
import SettingsIcon from '@mui/icons-material/Settings';
import PlaylistAddCheckIcon from '@mui/icons-material/PlaylistAddCheck';
import TerminalIcon from '@mui/icons-material/Terminal';

const DRAWER_WIDTH = 220;

const navItems = [
  { to: '/', label: 'Dashboard', icon: <DashboardIcon /> },
  { to: '/sources', label: 'Sources', icon: <SourceIcon /> },
  { to: '/processing', label: 'Processing', icon: <PlaylistAddCheckIcon /> },
  { to: '/outputs', label: 'Outputs', icon: <OutputIcon /> },
  { to: '/jobs', label: 'Jobs', icon: <WorkIcon /> },
  { to: '/logs', label: 'LLM Logs', icon: <TerminalIcon /> },
  { to: '/settings', label: 'Settings', icon: <SettingsIcon /> },
];

export default function Layout() {
  const [mobileOpen, setMobileOpen] = useState(false);
  const theme = useTheme();
  const isMobile = useMediaQuery(theme.breakpoints.down('sm'));
  const location = useLocation();

  const drawerContent = (
    <List sx={{ pt: 1 }}>
      {navItems.map((item) => {
        const active = item.to === '/' ? location.pathname === '/' : location.pathname.startsWith(item.to);
        return (
          <ListItemButton
            key={item.to}
            component={NavLink}
            to={item.to}
            selected={active}
            onClick={() => setMobileOpen(false)}
            sx={{
              mx: 1,
              borderRadius: 1,
              mb: 0.5,
              '&.active': {
                bgcolor: 'primary.main',
                color: 'primary.contrastText',
                '& .MuiListItemIcon-root': { color: 'primary.contrastText' },
                '&:hover': {
                  bgcolor: 'primary.dark',
                },
              },
            }}
          >
            <ListItemIcon sx={{ minWidth: 40 }}>{item.icon}</ListItemIcon>
            <ListItemText primary={item.label} />
          </ListItemButton>
        );
      })}
    </List>
  );

  return (
    <Box sx={{ display: 'flex' }}>
      <AppBar
        position="fixed"
        sx={{ zIndex: (t) => t.zIndex.drawer + 1 }}
      >
        <Toolbar>
          {isMobile && (
            <IconButton
              color="inherit"
              edge="start"
              onClick={() => setMobileOpen(!mobileOpen)}
              sx={{ mr: 2 }}
            >
              <MenuIcon />
            </IconButton>
          )}
          <Typography variant="h6" noWrap component="div" sx={{ flexGrow: 1 }}>
            lecture-distill
          </Typography>
        </Toolbar>
      </AppBar>

      {/* Mobile drawer */}
      {isMobile ? (
        <Drawer
          variant="temporary"
          open={mobileOpen}
          onClose={() => setMobileOpen(false)}
          ModalProps={{ keepMounted: true }}
          sx={{
            '& .MuiDrawer-paper': { width: DRAWER_WIDTH },
          }}
        >
          <Toolbar />
          {drawerContent}
        </Drawer>
      ) : (
        /* Desktop permanent drawer */
        <Drawer
          variant="permanent"
          sx={{
            width: DRAWER_WIDTH,
            flexShrink: 0,
            '& .MuiDrawer-paper': { width: DRAWER_WIDTH, boxSizing: 'border-box' },
          }}
        >
          <Toolbar />
          {drawerContent}
        </Drawer>
      )}

      <Box
        component="main"
        sx={{
          flexGrow: 1,
          p: 3,
          mt: 8,
          width: { sm: `calc(100% - ${DRAWER_WIDTH}px)` },
          minHeight: '100vh',
        }}
      >
        <Outlet />
      </Box>
    </Box>
  );
}
