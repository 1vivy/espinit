// SPDX-License-Identifier: GPL-2.0-only
#include "../bdsvars.c"
int esu_fs_init(void) { return 0; }
void esu_fs_exit(void) {}
static void guid_print(efi_guid_t *g) {
 u8 *b=g->b; printf("%08x-%04x-%04x-%02x%02x-",get_unaligned_le32(b),get_unaligned_le16(b+4),get_unaligned_le16(b+6),b[8],b[9]);
 for(int i=10;i<16;i++)printf("%02x",b[i]);
}
int main(int argc,char **argv) {
 efi_char16_t name[4096]={0}; efi_guid_t guid={{0x1c,0x4b,0x5e,0x7a,0x3f,0x0d,0x62,0x4e,0x9b,0x8a,0x1c,0x2d,0x3e,0x4f,0x5a,0x6b}};
 efi_status_t status; int ret; u64 capacity,remaining,maximum;
 if(argc<3)return 2;
 host_path=argv[1]; dev="8:16";
 if(!strcmp(argv[2],"missing-dev"))dev=NULL;
 ret=esu_init(); if(ret) { printf("init %d\n",ret); return 1; }
 if(!strcmp(argv[2],"list")) {
  unsigned long ns=sizeof(name);
  while((status=captured->get_next_variable(&ns,name,&guid))==EFI_SUCCESS) {
   unsigned long size=LIMIT; u32 attr; u8 *data=malloc(size);
   if(captured->get_variable(name,&guid,&attr,&size,data))return 3;
   guid_print(&guid); printf("  "); for(size_t i=0;name[i];i++)printf("%c",name[i]);
   printf("  0x%08x  %lu bytes\n",attr,size); free(data); ns=sizeof(name);
  }
  if(status!=EFI_NOT_FOUND)return 4;
 } else if(!strcmp(argv[2],"query")) {
  status=captured->query_variable_info(7,&capacity,&remaining,&maximum);
  printf("%lu %llu %llu %llu\n",status,(unsigned long long)capacity,(unsigned long long)remaining,(unsigned long long)maximum);
 } else {
  size_t size=0; u8 *data=NULL; u32 attr=0;
  if(argc<4)return 2;
  for(size_t i=0;argv[3][i] && i<4095;i++)name[i]=(u8)argv[3][i];
  if(strcmp(argv[2],"delete")) {
   FILE *f; long n;
   if(argc<6)return 2;
   attr=strtoul(argv[4],NULL,0); f=fopen(argv[5],"rb"); if(!f)return 2;
   fseek(f,0,SEEK_END);n=ftell(f);rewind(f);size=n;data=malloc(size ? size:1);
   if(fread(data,1,size,f)!=size)return 2;
   fclose(f);
  }
  corrupt_readback=!strcmp(argv[2],"corrupt");
  status=captured->set_variable(name,&guid,attr,size,data);
  printf("status %lu\n",status); free(data);
  if(corrupt_readback)return 5;
  if(!strcmp(argv[2],"corrupt")) {
   unsigned long n=LIMIT; u32 attributes; u8 *value=malloc(n);
   status=captured->get_variable(name,&guid,&attributes,&n,value);
   printf("reload %lu %u ",status,attributes);
   for(unsigned long i=0;status==EFI_SUCCESS && i<n;i++)printf("%02x",value[i]);
   printf("\n"); free(value);
  }
 }
 esu_exit(); return 0;
}
