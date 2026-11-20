<script lang='ts'>
  import Heart from '@lucide/svelte/icons/heart'
  import Play from '@lucide/svelte/icons/play'

  import { BannerImage } from '../banner'
  import { Button } from '../button'

  import SidebarButton from './SidebarButton.svelte'

  import { page } from '#lib/app-state'
  import StatusDot from '#lib/components/StatusDot.svelte'
  import Logo from '#lib/components/icons/Logo.svelte'
  import { Home, Search, Calendar, Users, Download, Bolt, LogIn, Messages } from '#lib/components/icons/animated'
  import * as Avatar from '#lib/components/ui/avatar'
  import client from '#lib/modules/auth/client'
  import { lockedState, idleState, activityState } from '#lib/modules/idle'
  import { irc } from '#lib/modules/irc/lobby'
  import native from '#lib/modules/native'
  import SUPPORTS from '#lib/modules/settings/supports'
  import { w2globby } from '#lib/modules/w2g/lobby'
  import { breakpoints, cn, highEntropyValues } from '#lib/utils'
  import { goto } from '$app/navigation'

  let visibilityState: DocumentVisibilityState

  $: active = ($lockedState === 'locked' || visibilityState === 'hidden' || ($idleState === 'active' && $activityState === 'active')) && $page.route.id !== '/app/player' && !SUPPORTS.isUnderPowered

  let isMac = false

  if (highEntropyValues) highEntropyValues.then(({ platform }) => { isMac = platform === 'macOS' })

  const viewer = client.viewer

  let size: 'default' | 'icon-lg' = 'default'

  $: size = ($breakpoints.md ? 'default' : 'icon-lg')
</script>

<svelte:document bind:visibilityState />

<BannerImage class='absolute top-0 left-0 w-14 -z-10 hidden md:block' />
<Logo class={cn('mb-1 h-10 object-contain px-2.5 hidden md:block text-foreground ml-2 cursor-pointer', isMac && 'mt-3')} on:click={() => goto('/#/app/home')} />
{#if SUPPORTS.isAndroidTV}
  <SidebarButton href='/#/app/player' class='hidden md:flex py-0'>
    <Play size={16} />
  </SidebarButton>
{/if}
<SidebarButton href='/#/app/home' class='animated-icon' {size}>
  <Home size={18} />
</SidebarButton>
<SidebarButton href='/#/app/search' class='animated-icon' {size}>
  <Search size={18} />
</SidebarButton>
<SidebarButton href='/#/app/schedule' class='animated-icon' {size}>
  <Calendar size={18} />
</SidebarButton>
<SidebarButton href='/#/app/w2g' class='animated-icon' {size}>
  <Users size={18} />
  {#if $w2globby}
    <StatusDot class='top-1 right-1 absolute !me-0' variant='COMPLETED' />
  {/if}
</SidebarButton>
<SidebarButton href='/#/app/chat' class='animated-icon' {size}>
  <Messages size={18} />
  {#if $irc}
    <StatusDot class='top-1 right-1 absolute !me-0' variant='COMPLETED' />
  {/if}
</SidebarButton>
<SidebarButton href='/#/app/client' class='animated-icon' {size}>
  <Download size={18} />
</SidebarButton>

<!-- <Dialog.Root portal='#root' >
  <Dialog.Trigger asChild let:builder>
    <Button variant='ghost' class='animated-icon px-2 w-10 relative md:pl-4 md:w-12 md:rounded-l-none hidden md:flex' builders={[builder]}>
      <Newspaper size={18} />
      <span class='inline-flex size-2 top-1 right-1 absolute rounded-full bg-red-500' />
    </Button>
  </Dialog.Trigger>
  <Dialog.Content class='max-w-5xl flex flex-col !w-[500px]'>
    <Dialog.Title class='font-bold text-2xl'>News</Dialog.Title>
    <Dialog.Description>
      Hayase's Discord server is now publicly available!<br /><br />If you want to recieve news and updates about the app you can join via the invite link on the official website.
    </Dialog.Description>
  </Dialog.Content>
</Dialog.Root> -->
<Button variant='ghost' {size} on:click={() => native.openURL('https://github.com/sponsors/ThaUnknown/')} class='contain-strict px-2 w-full relative mt-auto select:!bg-transparent text-[#fa68b6] select:text-[#fa68b6] md:pl-4 md:w-12 md:rounded-l-none'>
  <Heart size={18} fill='currentColor' class={cn('drop-shadow-[0_0_0.55rem_#fa68b6aa]', active && 'animate-[hearbeat_1s_ease-in-out_infinite_alternate]')} />
</Button>
<SidebarButton href='/#/app/settings' class='animated-icon !transition-none' {size}>
  <Bolt size={18} />
</SidebarButton>
<SidebarButton href='/#/app/profile' class='hidden md:flex animated-icon' {size}>
  <!-- <SidebarButton href='/#/app/profile' class='hidden md:flex py-0 animated-icon'> -->
  {#if $viewer}
    <Avatar.Root class='size-6 rounded-md'>
      <Avatar.Image src={$viewer.avatar?.large ?? ''} alt={$viewer.name} />
      <Avatar.Fallback>{$viewer.name}</Avatar.Fallback>
    </Avatar.Root>
  {:else}
    <LogIn size={18} />
  {/if}
</SidebarButton>
