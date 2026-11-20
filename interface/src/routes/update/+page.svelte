<script lang='ts'>
  import { Button } from '#lib/components/ui/button'
  import { Menubar } from '#lib/components/ui/menubar'
  import { Separator } from '#lib/components/ui/separator'
</script>

<div class='size-full flex flex-col grow items-center justify-center gap-9'>
  <Menubar />
  <div class='overflow-x-hidden overflow-y-scroll relative flex justify-center items-center text-foreground px-15 w-full font-light'>
    <div class='max-w-full min-w-96 justify-center items-center flex-col flex'>
      <div class='text-6xl text-center font-bold'>Up to Date</div>
      <Separator class='my-6 w-40' />
      <div class='text-xl text-wrap max-w-full text-center mb-6'>
        In-app updates are disabled for this build.
      </div>
      <Button class='text-md font-bold' size='lg' href='/#/app/home'>Continue</Button>
    </div>
  </div>
</div>
